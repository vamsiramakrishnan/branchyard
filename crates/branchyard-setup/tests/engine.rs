//! The engine end to end over a fake probe: every topic walked the way a
//! harness walks it, batch by batch; the shape guarantees the protocol
//! makes to a harness; determinism; and golden first batches.
//!
//! Regenerate the golden files with `BRANCHYARD_BLESS=1 cargo test -p
//! branchyard-setup --test engine`.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
use std::collections::BTreeMap;
use std::path::Path;

use branchyard_setup::probe::{CountingEntropy, FakeProbe};
use branchyard_setup::{next, BuiltinValidator, Kind, Question, Response, Topic};
use serde_json::{json, Value};

fn step(topic: Topic, probe: &FakeProbe, answers: &BTreeMap<String, Value>) -> Response {
    next(
        topic,
        probe,
        answers,
        false,
        &mut CountingEntropy(0),
        &BuiltinValidator,
    )
}

/// What a harness mapping a question onto `AskUserQuestion` relies on.
fn check_shape(q: &Question) {
    assert!(
        !q.id.is_empty() && q.header.chars().count() <= 12,
        "{}: header {:?}",
        q.id,
        q.header
    );
    assert!(q.prompt.ends_with('?'), "{}: {:?}", q.id, q.prompt);
    assert!(!q.why.is_empty(), "{}", q.id);
    assert!(
        !q.choices.is_empty() && q.choices.len() <= 4,
        "{}: {} choices",
        q.id,
        q.choices.len()
    );
    let mut labels: Vec<String> = q
        .choices
        .iter()
        .chain(&q.more_choices)
        .map(|c| c.label.to_lowercase())
        .collect();
    for label in &labels {
        assert!(
            !label.contains(','),
            "{}: label {label:?} has a comma",
            q.id
        );
    }
    let count = labels.len();
    labels.sort();
    labels.dedup();
    assert_eq!(labels.len(), count, "{}: duplicate labels", q.id);
    if q.kind == Kind::Select && !q.allow_other {
        assert!(
            q.choices
                .iter()
                .chain(&q.more_choices)
                .any(|c| c.value == q.default),
            "{}",
            q.id
        );
    }
    if q.kind == Kind::SecretRef {
        assert!(q.secret.is_some(), "{}", q.id);
    }
}

/// A harness that answers every question with its recommended (first)
/// choice's label, as a person clicking through would, and loops.
fn walk(topic: Topic, probe: &FakeProbe, mut answers: BTreeMap<String, Value>) -> Response {
    for round in 0..20 {
        let response = step(topic, probe, &answers);
        assert!(
            response.errors.is_empty(),
            "{topic:?} round {round}: {:?}",
            response.errors
        );
        if response.done {
            return response;
        }
        assert!(!response.questions.is_empty() && response.questions.len() <= 4);
        let ids: Vec<&str> = response.questions.iter().map(|q| q.id.as_str()).collect();
        for q in &response.questions {
            check_shape(q);
            // No question in a batch depends on another in it.
            if let Some(when) = &q.when {
                for id in when.ids() {
                    assert!(
                        !ids.contains(&id),
                        "{} depends on {id} in the same batch",
                        q.id
                    );
                }
            }
            answers.insert(q.id.clone(), Value::String(q.choices[0].label.clone()));
        }
        // What a harness sends back next: the normalized answers too.
        for (id, value) in response.answers {
            answers.entry(id).or_insert(value);
        }
    }
    panic!("{topic:?} did not finish in 20 rounds");
}

#[test]
fn every_topic_finishes_with_a_valid_plan_when_answered_by_label() {
    let probe = FakeProbe::typical();
    for topic in Topic::ALL {
        let done = walk(topic, &probe, BTreeMap::new());
        let plan = done.plan.expect("done has a plan");
        assert!(plan.valid, "{topic:?}: {:#?}", plan.files);
        assert!(
            !plan.summary.is_empty() && !plan.commands.is_empty(),
            "{topic:?}"
        );
        assert_eq!(done.remaining, 0);
    }
}

#[test]
fn defaults_finish_every_topic_in_one_step() {
    let probe = FakeProbe::typical();
    for topic in Topic::ALL {
        let response = next(
            topic,
            &probe,
            &BTreeMap::new(),
            true,
            &mut CountingEntropy(0),
            &BuiltinValidator,
        );
        assert!(response.done, "{topic:?}: {:?}", response.questions);
        assert!(response.plan.unwrap().valid, "{topic:?}");
    }
}

#[test]
fn the_same_answers_give_the_same_bytes() {
    let probe = FakeProbe::typical();
    for topic in Topic::ALL {
        let a = serde_json::to_string(&next(
            topic,
            &probe,
            &BTreeMap::new(),
            true,
            &mut CountingEntropy(0),
            &BuiltinValidator,
        ))
        .unwrap();
        let b = serde_json::to_string(&next(
            topic,
            &probe,
            &BTreeMap::new(),
            true,
            &mut CountingEntropy(0),
            &BuiltinValidator,
        ))
        .unwrap();
        assert_eq!(a, b, "{topic:?}");
    }
}

#[test]
fn no_generated_secret_appears_in_any_response() {
    let probe = FakeProbe::typical();
    for topic in Topic::ALL {
        let json = serde_json::to_string(&next(
            topic,
            &probe,
            &BTreeMap::new(),
            true,
            &mut CountingEntropy(0),
            &BuiltinValidator,
        ))
        .unwrap();
        assert!(
            !json.contains("test-token-"),
            "{topic:?} leaked a generated secret"
        );
    }
}

#[test]
fn a_pasted_secret_is_refused_without_being_repeated() {
    let probe = FakeProbe::typical();
    let answers: BTreeMap<String, Value> = serde_json::from_value(json!({
        "isolated": true,
        "secrets": ["ANTHROPIC_API_KEY"],
        "secret.ANTHROPIC_API_KEY": "sk-ant-api03-PASTEDVALUE0123456789",
    }))
    .unwrap();
    // Everything else defaulted: the refused question is all that is left.
    let response = next(
        Topic::Project,
        &probe,
        &answers,
        true,
        &mut CountingEntropy(0),
        &BuiltinValidator,
    );
    assert!(!response.done);
    let json = serde_json::to_string(&response).unwrap();
    assert!(!json.contains("PASTEDVALUE"), "{json}");
    assert!(response
        .errors
        .iter()
        .any(|e| e.id == "secret.ANTHROPIC_API_KEY"));
    assert!(response
        .questions
        .iter()
        .any(|q| q.id == "secret.ANTHROPIC_API_KEY"));
}

#[test]
fn detection_changes_questions_and_defaults() {
    let mut probe = FakeProbe::typical();
    let first = step(Topic::Project, &probe, &BTreeMap::new());
    let harness = first.questions.iter().find(|q| q.id == "harness").unwrap();
    assert_eq!(harness.default, json!("claude-code"));
    assert!(harness.choices[0]
        .description
        .contains("installed: 2.1.283"));

    // Only Codex installed: it becomes the default; no KVM, no provider question.
    probe.harnesses.retain(|h| h.harness != "claude-code");
    let done = next(
        Topic::Project,
        &probe,
        &BTreeMap::new(),
        true,
        &mut CountingEntropy(0),
        &BuiltinValidator,
    );
    assert_eq!(done.answers["harness"], json!("codex"));
    assert!(!done.answers.contains_key("provider"));

    // KVM adds the provider question.
    probe.platform.kvm = true;
    let done = next(
        Topic::Project,
        &probe,
        &BTreeMap::new(),
        true,
        &mut CountingEntropy(0),
        &BuiltinValidator,
    );
    assert_eq!(done.answers["provider"], json!("local"));

    // An existing branchyard.toml supplies the defaults and is diffed.
    probe.files.insert(
        "branchyard.toml".into(),
        "version = 1\n[defaults]\nmax_turns = 7\n[mcp]\ndocs = \"/bin/docs\"\n".into(),
    );
    let done = next(
        Topic::Project,
        &probe,
        &BTreeMap::new(),
        true,
        &mut CountingEntropy(0),
        &BuiltinValidator,
    );
    assert_eq!(done.answers["max_turns"], json!(7));
    let plan = done.plan.unwrap();
    let file = &plan.files[0];
    assert!(file.overwrites);
    assert!(
        file.content
            .as_deref()
            .unwrap()
            .contains("docs = \"/bin/docs\""),
        "unasked keys are kept"
    );
    assert!(file
        .diff
        .as_deref()
        .unwrap()
        .contains("--- a/branchyard.toml"));
}

fn golden(name: &str, value: &Response) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    let text = serde_json::to_string_pretty(value).unwrap() + "\n";
    if std::env::var_os("BRANCHYARD_BLESS").is_some() {
        std::fs::write(&path, &text).unwrap();
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        expected == text,
        "{name} differs; regenerate with BRANCHYARD_BLESS=1 cargo test -p branchyard-setup --test engine\n{text}"
    );
}

#[test]
fn first_batches_match_the_golden_files() {
    let probe = FakeProbe::typical();
    golden(
        "project-first.json",
        &step(Topic::Project, &probe, &BTreeMap::new()),
    );
    golden(
        "server-first.json",
        &step(Topic::Server, &probe, &BTreeMap::new()),
    );
    let answers: BTreeMap<String, Value> = serde_json::from_value(json!({
        "path": ".branchyard/server.json", "listen": "0.0.0.0:8421", "database": "PostgreSQL", "tenancy": "multi"
    }))
    .unwrap();
    golden("server-second.json", &step(Topic::Server, &probe, &answers));
}

/// The batch that asks about `[workspace]`, in a repository whose files
/// suggest one: its defaults come from what was detected.
#[test]
fn the_workspace_batch_matches_its_golden_file() {
    let mut probe = FakeProbe::typical();
    for file in [
        "package.json",
        "pnpm-lock.yaml",
        ".env",
        ".env.local",
        "compose.yaml",
    ] {
        probe.files.insert(file.into(), String::new());
    }
    let mut answers = BTreeMap::new();
    for _ in 0..20 {
        let response = step(Topic::Project, &probe, &answers);
        if response.questions.iter().any(|q| q.id == "workspace.copy") {
            golden("project-workspace.json", &response);
            return;
        }
        assert!(!response.done, "the workspace was never asked about");
        for q in &response.questions {
            answers.insert(q.id.clone(), q.default.clone());
        }
    }
    panic!("the workspace was never asked about");
}
