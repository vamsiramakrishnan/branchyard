//! Result contracts against the fake ACP agent: a parent says at spawn what
//! shape a child's answer must take, the child reports a result that is
//! checked before anything is recorded, and the parent reads it back as
//! JSON through `inspect` and `wait`. The MCP tools and a harness calling
//! them are tested end to end in `branchyard-mcp`.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::time::Duration;

use branchyard::{
    Activity, Branch, BranchStatus, Delegate, Envelope, Error, GraphEdit, MessageKind, Policy,
    ResultContract, Spawn, SpawnSpec, TaskOptions,
};
use branchyard_testkit::wait;
use common::{fake_agent, Fixture};
use serde_json::{json, Value};

fn delegating(f: &Fixture) -> TaskOptions {
    TaskOptions {
        delegation: Some(Envelope::depth(2)),
        delegation_server: Some(vec![fake_agent().display().to_string()]),
        policy: Policy::allow_all(),
        ..f.options()
    }
}

/// `root`, with one finished turn, and its delegate.
fn root(f: &Fixture, options: &TaskOptions) -> (Branch, Delegate) {
    let root = f
        .yard
        .task("say hi")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let delegate = root.delegate(options.clone()).unwrap();
    (root, delegate)
}

fn contract(required: bool) -> ResultContract {
    ResultContract::new(
        json!({
            "type": "object",
            "required": ["files", "ok"],
            "additionalProperties": false,
            "properties": {
                "files": {"type": "array", "items": {"type": "string"}},
                "ok": {"type": "boolean"}
            }
        }),
        required,
    )
    .unwrap()
}

fn spawn(prompt: &str, name: &str, result: Option<ResultContract>) -> Spawn {
    Spawn {
        prompt: prompt.into(),
        name: Some(name.into()),
        result,
        ..Spawn::default()
    }
}

fn child(f: &Fixture, options: &TaskOptions, name: &str) -> Delegate {
    f.yard
        .branch(name)
        .unwrap()
        .delegate(options.clone())
        .unwrap()
}

fn log(f: &Fixture, branch: &str) -> Vec<branchyard::RecordedEvent> {
    f.yard.branch(branch).unwrap().events().unwrap()
}

fn warnings(f: &Fixture, branch: &str) -> Vec<String> {
    log(f, branch)
        .into_iter()
        .filter_map(|e| match e.activity {
            Activity::Warning(text) => Some(text),
            _ => None,
        })
        .collect()
}

fn denied(result: Result<impl std::fmt::Debug, Error>, needle: &str) {
    match result {
        Err(Error::Denied(why)) if why.contains(needle) => {}
        other => panic!("expected a denial mentioning {needle:?}, got {other:?}"),
    }
}

/// A child that waits for steering, so its turn ends when its parent says.
fn spawn_waiting(f: &Fixture, parent: &Delegate, name: &str, result: Option<ResultContract>) {
    parent.spawn(spawn("AWAIT_STEER", name, result)).unwrap();
    wait::until(&format!("{name} to wait for steering"), || {
        parent
            .inspect(name)
            .is_ok_and(|i| i.last_message.contains("waiting for steering"))
    });
    assert_eq!(f.yard.branch(name).unwrap().info().name, name);
}

/// The result a child reports is recorded on its log before the message
/// that carries it, is its parent's `inspect` result and each settled
/// child's in `wait`, and the latest report wins.
#[test]
fn a_matching_result_is_read_by_the_parent_through_inspect_and_wait() {
    let f = Fixture::new();
    let options = delegating(&f);
    let (root, parent) = root(&f, &options);
    let wanted = contract(true);
    spawn_waiting(&f, &parent, "kid", Some(wanted.clone()));
    let kid = child(&f, &options, "kid");

    // The contract shows to the parent and to the child, with no result yet.
    for reader in [&parent, &kid] {
        let seen = reader.inspect("kid").unwrap();
        assert_eq!(seen.result_contract, Some(wanted.clone()));
        assert_eq!(seen.result, None);
    }

    let value = json!({"files": ["a.rs", "b.rs"], "ok": true});
    let message = kid.report_result("found two files", &value).unwrap();
    assert_eq!(message.kind, MessageKind::Report);
    assert_eq!(message.text, "found two files");
    let seen = parent.inspect("kid").unwrap();
    assert_eq!(seen.result, Some(value.clone()));
    assert_eq!(seen.status, BranchStatus::Running, "a result is no status");
    let wire = serde_json::to_value(&seen).unwrap();
    assert_eq!(wire["result"], value);
    assert_eq!(
        wire["result_contract"],
        json!({"schema": wanted.schema, "required": true})
    );

    // On the child's log as `result`, ahead of the message that carries it.
    let events = log(&f, "kid");
    let at = |wanted: &dyn Fn(&Activity) -> bool| events.iter().position(|e| wanted(&e.activity));
    let recorded = at(&|a| matches!(a, Activity::Result { value: v } if *v == value)).unwrap();
    let sent = at(&|a| matches!(a, Activity::Message(m) if m.id == message.id)).unwrap();
    assert!(recorded < sent, "the result is recorded before the message");
    assert_eq!(parent.inbox().unwrap().messages.len(), 1);

    // A later report replaces it; the log keeps both.
    let newer = json!({"files": [], "ok": false});
    kid.report_result("none after all", &newer).unwrap();
    assert_eq!(parent.inspect("kid").unwrap().result, Some(newer.clone()));
    let results: Vec<Value> = log(&f, "kid")
        .into_iter()
        .filter_map(|e| match e.activity {
            Activity::Result { value } => Some(value),
            _ => None,
        })
        .collect();
    assert_eq!(results, [value, newer.clone()]);

    // Let the turn end: `wait` returns the result with the settled child.
    parent.steer("kid", "finish").unwrap();
    let waited = parent
        .wait_for(&["kid"], false, Some(Duration::from_secs(60)))
        .unwrap();
    assert!(waited.pending.is_empty() && !waited.timed_out);
    assert_eq!(waited.settled.len(), 1);
    assert_eq!(waited.settled[0].name, "kid");
    assert_eq!(waited.settled[0].status, BranchStatus::NoChanges);
    assert_eq!(waited.settled[0].result, Some(newer));
    assert_eq!(waited.settled[0].result_contract, Some(wanted));

    // It reported during its turn, so its parent's log has no warning.
    root.wait_subtree().unwrap();
    assert!(warnings(&f, "root")
        .iter()
        .all(|w| !w.contains("without the result")));
}

/// A result that does not match is refused naming where and why, with
/// nothing recorded and no message sent; a report without a result still
/// works when the contract does not require one.
#[test]
fn a_result_that_does_not_match_is_refused_naming_the_path_and_nothing_is_recorded() {
    let f = Fixture::new();
    let options = delegating(&f);
    let (root, parent) = root(&f, &options);
    parent
        .spawn(spawn("HANG", "kid", Some(contract(false))))
        .unwrap();
    wait::until("kid to start its turn", || {
        log(&f, "kid")
            .iter()
            .any(|e| matches!(e.activity, Activity::Prompt(_)))
    });
    let kid = child(&f, &options, "kid");

    for (value, needle) in [
        (
            json!({"files": ["a.rs", 3], "ok": true}),
            "result.files[1]: expected string, got number",
        ),
        (
            json!({"files": []}),
            "result: missing the required field \"ok\"",
        ),
        (
            json!({"files": [], "ok": "yes"}),
            "result.ok: expected boolean, got string",
        ),
        (
            json!({"files": [], "ok": true, "extra": 1}),
            "result: the field \"extra\" is not allowed",
        ),
        (json!("done"), "result: expected object, got string"),
    ] {
        denied(kid.report_result("done", &value), needle);
    }
    // Every violation of one value is named.
    match kid.report_result("done", &json!({"files": [1, 2], "ok": 0})) {
        Err(Error::Denied(why)) => assert_eq!(
            why,
            "result.files[0]: expected string, got number; \
             result.files[1]: expected string, got number; \
             result.ok: expected boolean, got number"
        ),
        other => panic!("expected a denial, got {other:?}"),
    }

    // Nothing was recorded, nothing was sent.
    assert!(log(&f, "kid")
        .iter()
        .all(|e| !matches!(e.activity, Activity::Result { .. } | Activity::Message(_))));
    assert!(parent.inbox().unwrap().messages.is_empty());
    assert_eq!(parent.inspect("kid").unwrap().result, None);

    // A message with no result is as before, and a good result is accepted.
    kid.report("halfway").unwrap();
    assert_eq!(parent.inbox().unwrap().messages.len(), 1);
    let good = json!({"files": ["a.rs"], "ok": true});
    kid.report_result("done", &good).unwrap();
    assert_eq!(parent.inspect("kid").unwrap().result, Some(good));

    parent.cancel("kid").unwrap();
    root.wait_subtree().unwrap();
}

/// A required contract refuses a report without a result, and a child whose
/// turn ends with none leaves a warning in its parent's log; a child that
/// stopped some other way says why in its status and leaves none.
#[test]
fn a_required_result_is_demanded_and_a_child_ending_without_one_is_noted_to_its_parent() {
    let f = Fixture::new();
    let options = delegating(&f);
    let (root, parent) = root(&f, &options);
    parent
        .spawn(spawn("HANG", "strict", Some(contract(true))))
        .unwrap();
    wait::until("strict to start its turn", || {
        log(&f, "strict")
            .iter()
            .any(|e| matches!(e.activity, Activity::Prompt(_)))
    });
    let strict = child(&f, &options, "strict");
    for result in [None, Some(Value::Null)] {
        let refused = match &result {
            None => strict.report("done"),
            Some(value) => strict.report_result("done", value),
        };
        denied(refused, "requires a result");
    }
    assert!(parent.inbox().unwrap().messages.is_empty(), "nothing sent");
    // Cancelled, it says so in its status, not in a warning.
    parent.cancel("strict").unwrap();
    root.wait_subtree().unwrap();
    assert_eq!(
        parent.inspect("strict").unwrap().status,
        BranchStatus::Interrupted
    );

    // One that finishes its turn without a result is noted.
    parent
        .spawn(spawn("say hi", "quiet", Some(contract(true))))
        .unwrap();
    root.wait_subtree().unwrap();
    wait::until("the missing result to be noted in the parent's log", || {
        warnings(&f, "root")
            .iter()
            .any(|w| w.starts_with("quiet ended its turn without the result its contract requires"))
    });
    let noted = warnings(&f, "root");
    assert_eq!(
        noted
            .iter()
            .filter(|w| w.contains("without the result"))
            .count(),
        1,
        "{noted:?}"
    );
    assert!(noted.iter().all(|w| !w.starts_with("strict ")), "{noted:?}");
    let quiet = parent.inspect("quiet").unwrap();
    assert_eq!(quiet.status, BranchStatus::NoChanges);
    assert_eq!(quiet.result, None);
    assert!(quiet.result_contract.unwrap().required);

    // An optional contract leaves no warning.
    parent
        .spawn(spawn("say hi", "relaxed", Some(contract(false))))
        .unwrap();
    root.wait_subtree().unwrap();
    assert!(warnings(&f, "root")
        .iter()
        .all(|w| !w.starts_with("relaxed ")));
}

/// Without a contract a result is recorded as it is, of any shape, and
/// nothing about the contract shows.
#[test]
fn a_result_without_a_contract_is_recorded_as_is() {
    let f = Fixture::new();
    let options = delegating(&f);
    let (root, parent) = root(&f, &options);
    parent.spawn(spawn("HANG", "free", None)).unwrap();
    wait::until("free to start its turn", || {
        log(&f, "free")
            .iter()
            .any(|e| matches!(e.activity, Activity::Prompt(_)))
    });
    let free = child(&f, &options, "free");
    let before = parent.inspect("free").unwrap();
    assert_eq!((before.result_contract, before.result), (None, None));

    // A null is no result: the message goes, nothing is recorded.
    free.report_result("nothing yet", &Value::Null).unwrap();
    assert_eq!(parent.inspect("free").unwrap().result, None);

    let odd = json!([1, "two", {"three": null}, [false]]);
    free.report_result("here", &odd).unwrap();
    let seen = parent.inspect("free").unwrap();
    assert_eq!(seen.result, Some(odd.clone()));
    assert_eq!(seen.result_contract, None);
    let wire = serde_json::to_value(&seen).unwrap();
    assert_eq!(wire["result"], odd);
    assert!(wire.get("result_contract").is_none(), "{wire}");

    // Too large a result is refused even so.
    denied(
        free.report_result("big", &json!("x".repeat(70_000))),
        "keep it to at most 65536",
    );
    parent.cancel("free").unwrap();
    root.wait_subtree().unwrap();
}

/// A schema keyword outside the subset is refused at spawn, by name, before
/// any child is created; so is one in a graph proposal's spawn. A valid
/// contract in a proposal reaches the child.
#[test]
fn a_contract_outside_the_subset_is_refused_at_spawn_and_creates_no_child() {
    let f = Fixture::new();
    let options = delegating(&f);
    let (_root, parent) = root(&f, &options);
    let unsupported = ResultContract {
        schema: json!({"type": "object", "properties": {"id": {"type": "string", "$ref": "#/a"}}}),
        required: false,
        purpose: branchyard::ResultPurpose::Report,
    };
    let needle = "result.schema.properties.id uses `$ref`, which the contract does not support";
    denied(
        parent.spawn(spawn("say hi", "bad", Some(unsupported.clone()))),
        needle,
    );
    assert!(f.yard.branch("bad").is_err(), "no child was created");
    assert!(f.yard.branch("root").unwrap().info().children.is_empty());

    let revision = parent.graph("root").unwrap().revision;
    let edit = |name: &str, result: Option<ResultContract>| {
        GraphEdit::Spawn(SpawnSpec {
            prompt: "say hi".into(),
            name: Some(name.into()),
            result,
            ..SpawnSpec::default()
        })
    };
    denied(
        parent.apply_graph(vec![edit("bad2", Some(unsupported))], revision),
        needle,
    );
    assert!(f.yard.branch("bad2").is_err());

    let applied = parent
        .apply_graph(vec![edit("good", Some(contract(true)))], revision)
        .unwrap();
    assert_eq!(applied.spawned.len(), 1);
    assert_eq!(
        parent.inspect("good").unwrap().result_contract,
        Some(contract(true))
    );
}

/// Every surface ends in the tools' JSON: `spawn` takes `result` as
/// `{schema, required}`, `report` takes `result` as any value, and a
/// misspelt field of the contract is refused.
#[test]
fn the_tools_take_the_contract_and_the_result_as_json() {
    let f = Fixture::new();
    let options = delegating(&f);
    let (root, parent) = root(&f, &options);
    let spawned = parent
        .call(
            "spawn",
            json!({
                "prompt": "HANG",
                "name": "kid",
                "result": {"schema": {"type": "array", "items": {"type": "integer"}}, "required": true},
            }),
        )
        .unwrap();
    assert_eq!(spawned["name"], "kid");
    wait::until("kid to start its turn", || {
        log(&f, "kid")
            .iter()
            .any(|e| matches!(e.activity, Activity::Prompt(_)))
    });
    let seen = parent.call("inspect", json!({"branch": "kid"})).unwrap();
    assert_eq!(
        seen["result_contract"],
        json!({"schema": {"type": "array", "items": {"type": "integer"}}, "required": true})
    );
    assert!(seen.get("result").is_none());

    let kid = child(&f, &options, "kid");
    match kid.call("report", json!({"text": "x", "result": [1, "2"]})) {
        Err(Error::Denied(why)) => assert_eq!(why, "result[1]: expected integer, got string"),
        other => panic!("expected a denial, got {other:?}"),
    }
    kid.call("report", json!({"text": "x", "result": [1, 2, 3.0]}))
        .unwrap();
    let seen = parent.call("inspect", json!({"branch": "kid"})).unwrap();
    assert_eq!(seen["result"], json!([1, 2, 3.0]));

    denied(
        parent.call(
            "spawn",
            json!({"prompt": "x", "result": {"schema": {}, "requried": true}}),
        ),
        "invalid arguments for spawn",
    );
    parent.cancel("kid").unwrap();
    root.wait_subtree().unwrap();
}

/// A contract using the richer keywords end to end: refused at spawn when
/// one is declared wrong, a matching result accepted and a non-matching one
/// refused naming the place, and `null` a result, read back as `null`
/// through the tools' JSON, because the schema accepts it.
#[test]
fn a_contract_with_the_richer_keywords_and_a_null_result_works_end_to_end() {
    let f = Fixture::new();
    let options = delegating(&f);
    let (root, parent) = root(&f, &options);
    let bad = ResultContract {
        schema: json!({"properties": {"id": {"pattern": "(x"}}}),
        required: false,
        purpose: branchyard::ResultPurpose::Report,
    };
    denied(
        parent.spawn(spawn("say hi", "bad", Some(bad))),
        "result.schema.properties.id.pattern is not a valid regular expression: unclosed group",
    );
    assert!(f.yard.branch("bad").is_err(), "no child was created");

    let wanted = ResultContract::new(
        json!({
            "title": "Findings, or null when there are none",
            "oneOf": [
                {"type": "null"},
                {
                    "type": "object",
                    "required": ["id", "files"],
                    "properties": {
                        "id": {"type": "string", "pattern": "^[A-Z]+-[0-9]+$"},
                        "files": {"type": "array", "minItems": 1, "uniqueItems": true,
                                  "items": {"type": "string"}},
                        "score": {"exclusiveMinimum": 0, "exclusiveMaximum": 1},
                        "kind": {"const": "bug"}
                    }
                }
            ]
        }),
        true,
    )
    .unwrap();
    spawn_waiting(&f, &parent, "kid", Some(wanted));
    let kid = child(&f, &options, "kid");

    let good = json!({"id": "BY-7", "files": ["a.rs"], "score": 0.5, "kind": "bug"});
    kid.report_result("found one", &good).unwrap();
    assert_eq!(parent.inspect("kid").unwrap().result, Some(good.clone()));
    let Err(Error::Denied(why)) =
        kid.report_result("again", &json!({"id": "by-7", "files": ["a.rs", "a.rs"]}))
    else {
        panic!("a result that does not match was accepted");
    };
    assert!(
        why.starts_with("result: matches none of the 2 alternatives in oneOf (closest, oneOf[1]: "),
        "{why}"
    );
    assert!(
        why.contains("result.files[1]: repeats the item at [0]"),
        "{why}"
    );
    assert!(
        why.contains("result.id: \"by-7\" does not match the pattern \"^[A-Z]+-[0-9]+$\""),
        "{why}"
    );
    assert_eq!(parent.inspect("kid").unwrap().result, Some(good));

    // Null is a result here, through the tool's JSON as through the API,
    // and it satisfies `required`.
    kid.call("report", json!({"text": "none after all", "result": null}))
        .unwrap();
    assert_eq!(parent.inspect("kid").unwrap().result, Some(Value::Null));
    let seen = parent.call("inspect", json!({"branch": "kid"})).unwrap();
    assert_eq!(seen.get("result"), Some(&Value::Null), "{seen}");
    let read: branchyard::Inspection = serde_json::from_value(seen).unwrap();
    assert_eq!(read.result, Some(Value::Null));
    // A report with no result at all is still refused: it is required.
    denied(kid.report("done"), "requires a result");

    parent.steer("kid", "finish").unwrap();
    let waited = parent
        .wait_for(&["kid"], false, Some(Duration::from_secs(60)))
        .unwrap();
    assert_eq!(waited.settled[0].result, Some(Value::Null));
    root.wait_subtree().unwrap();
    assert!(warnings(&f, "root")
        .iter()
        .all(|w| !w.contains("without the result")));
}

/// An explicit in-turn report wins over the native final prose, with no retry.
#[test]
fn an_explicit_deliverable_report_is_not_replaced_by_native_final_output() {
    let f = Fixture::new();
    let options = delegating(&f);
    let (_root, parent) = root(&f, &options);
    let wanted = ResultContract::deliverable(serde_json::json!({"type":"object", "required":["answer"], "properties":{"answer":{"type":"string"}}})).unwrap();
    spawn_waiting(&f, &parent, "answer", Some(wanted));
    let kid = child(&f, &options, "answer");
    kid.report_result(
        "explicit evidence",
        &serde_json::json!({"answer":"accepted report"}),
    )
    .unwrap();
    parent.steer("answer", "finish with prose").unwrap();
    let waited = parent
        .wait_for(&["answer"], false, Some(Duration::from_secs(60)))
        .unwrap();
    assert!(!waited.timed_out);
    assert_eq!(
        waited.settled[0].result,
        Some(serde_json::json!({"answer":"accepted report"}))
    );
    let events = log(&f, "answer");
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.activity, Activity::Result { .. }))
            .count(),
        1
    );
    assert!(warnings(&f, "answer")
        .iter()
        .all(|w| !w.contains("native final answer")));
}
