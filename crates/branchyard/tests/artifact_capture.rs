//! Producer-bound outbox collection over a hermetic fake harness; no model or cloud.

#![allow(clippy::unwrap_used, clippy::panic)] // tests: a panic is the failure report
mod common;

use std::fs;
use std::sync::Arc;

use branchyard::{
    BranchStatus, Provider, ResultCapture, ResultContract, SandboxOptions, TaskOptions,
};
use branchyard_runtime::LocalProvider;
use branchyard_sandbox::fake::FakeProvider;
use branchyard_testkit::wait;
use common::Fixture;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

fn contract() -> ResultContract {
    let mut contract = ResultContract::deliverable(json!({
        "type":"object", "required":["answer"], "additionalProperties":false,
        "properties":{"answer":{"type":"string"}}
    }))
    .unwrap();
    contract.capture = Some(ResultCapture::new("outputs/artifacts.json"));
    contract
}

fn manifest() -> Value {
    json!({"schema_version":1,"files":[{"path":"evidence.txt","media_type":"text/plain",
        "sha256":hex::encode(Sha256::digest(b"checked evidence"))}]})
}

fn script(f: &Fixture, name: &str, manifest: Option<Value>, file_setup: &str) -> String {
    let path = f.dir.join(format!("{name}.sh"));
    let mut source = format!("set -eu\nmkdir -p outputs\n{file_setup}\n");
    if let Some(manifest) = manifest {
        source.push_str(&format!(
            "printf %s {} > outputs/artifacts.json\n",
            branchyard_recipe::quote(&manifest.to_string())
        ));
    }
    source.push_str("printf '\\n```json\\n{\"answer\":\"checked\"}\\n```\\n'\n");
    fs::write(&path, source).unwrap();
    format!(
        "Produce evidence\nSH sh {}",
        branchyard_recipe::quote(path.to_str().unwrap())
    )
}

fn options(f: &Fixture, contract: ResultContract) -> TaskOptions {
    TaskOptions {
        result: Some(contract),
        ..f.options()
    }
}

#[test]
fn ignored_local_outputs_complete_without_git_changes_and_do_not_change_result_json() {
    let f = Fixture::new();
    fs::write(f.root.join(".gitignore"), "outputs/\n").unwrap();
    f.git(&["add", ".gitignore"]);
    f.git(&["commit", "-qm", "Ignore document outbox"]);
    let prompt = script(
        &f,
        "local",
        Some(manifest()),
        "printf %s 'checked evidence' > outputs/evidence.txt",
    );
    let branch = f
        .yard
        .task(prompt)
        .options(options(&f, contract()))
        .name("local")
        .run()
        .unwrap();
    assert_eq!(branch.info().status, BranchStatus::NoChanges);
    assert!(branch.info().candidate.is_none());
    assert_eq!(
        branchyard::contract::current(&branch.events().unwrap()),
        Some(json!({"answer":"checked"}))
    );
    let artifacts = f.yard.artifacts("local").unwrap();
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].publisher_branch, "local");
    assert_eq!(artifacts[0].labels["capture.path"], "evidence.txt");
    let copied = f.dir.join("copied.txt");
    f.yard
        .read_artifact("local", &artifacts[0].id, &copied)
        .unwrap();
    assert_eq!(fs::read(copied).unwrap(), b"checked evidence");
}

#[test]
fn shared_mount_fake_sandbox_outputs_are_collected_only_after_its_harness_finishes() {
    let f = Fixture::new();
    f.yard.use_sandbox_provider(Arc::new(FakeProvider::plain(
        Box::new(LocalProvider::new()),
        f.dir.join("sandbox"),
    )));
    f.yard.use_sandbox_host(branchyard::SandboxHost::loopback());
    let mut options = options(&f, contract());
    options.provider = Some(Provider::Microsandbox(SandboxOptions {
        image: "fixture.example/artifacts:v1".into(),
        ..SandboxOptions::default()
    }));
    let prompt = script(
        &f,
        "sandbox",
        Some(manifest()),
        "printf %s 'checked evidence' > outputs/evidence.txt",
    );
    let branch = f
        .yard
        .task(prompt)
        .options(options)
        .name("sandbox")
        .run()
        .unwrap();
    assert!(matches!(
        branch.info().status,
        BranchStatus::Ready | BranchStatus::NoChanges
    ));
    assert_eq!(f.yard.artifacts("sandbox").unwrap().len(), 1);
    assert_eq!(
        branchyard::contract::current(&branch.events().unwrap()),
        Some(json!({"answer":"checked"}))
    );
}

#[test]
fn missing_invalid_oversize_traversing_and_digest_mismatched_outboxes_fail_closed() {
    let f = Fixture::new();
    let mut cases = Vec::new();
    cases.push(("missing", None, contract(), "true".to_owned()));
    cases.push((
        "empty",
        Some(json!({"schema_version":1,"files":[]})),
        contract(),
        "true".into(),
    ));
    cases.push((
        "manifest-limit",
        Some(json!({"schema_version":1,"files":[],"padding":"x".repeat(1024*1024)})),
        contract(),
        "true".into(),
    ));
    for (name, path) in [
        ("traversal", "../outside.txt"),
        ("absolute", "/outside.txt"),
    ] {
        let mut value = manifest();
        value["files"][0]["path"] = json!(path);
        cases.push((name, Some(value), contract(), "true".into()));
    }
    let mut wrong_digest = manifest();
    wrong_digest["files"][0]["sha256"] = json!("0".repeat(64));
    cases.push((
        "digest",
        Some(wrong_digest),
        contract(),
        "printf %s 'checked evidence' > outputs/evidence.txt".into(),
    ));
    let mut oversized = contract();
    oversized.capture.as_mut().unwrap().max_bytes = 3;
    cases.push((
        "oversize",
        Some(manifest()),
        oversized,
        "printf %s 'checked evidence' > outputs/evidence.txt".into(),
    ));
    let mut unknown = manifest();
    unknown["actor"] = json!("someone-else");
    cases.push(("unknown", Some(unknown), contract(), "true".into()));
    let mut duplicate = manifest();
    duplicate["files"] = json!([duplicate["files"][0].clone(), duplicate["files"][0].clone()]);
    cases.push((
        "duplicate",
        Some(duplicate),
        contract(),
        "printf %s 'checked evidence' > outputs/evidence.txt".into(),
    ));
    cases.push((
        "symlink",
        Some(manifest()),
        contract(),
        "ln -s /etc/hosts outputs/evidence.txt".into(),
    ));
    cases.push((
        "special",
        Some(manifest()),
        contract(),
        "mkfifo outputs/evidence.txt".into(),
    ));
    for (name, value, contract, setup) in cases {
        let prompt = script(&f, name, value, &setup);
        assert!(
            f.yard
                .task(prompt)
                .options(options(&f, contract))
                .name(name)
                .run()
                .is_err(),
            "{name}"
        );
        let branch = f.yard.branch(name).unwrap();
        assert!(
            matches!(branch.info().status, BranchStatus::Failed { .. }),
            "{name}: {:?}",
            branch.info().status
        );
        assert!(f.yard.artifacts(name).unwrap().is_empty(), "{name}");
        assert_eq!(
            branchyard::contract::current(&branch.events().unwrap()),
            None,
            "{name}"
        );
    }
}

#[test]
fn capture_is_explicit_and_restricted_to_required_deliverables() {
    let mut report = ResultContract::new(json!({"type":"object"}), true).unwrap();
    report.capture = Some(ResultCapture::new("outputs/artifacts.json"));
    assert!(report.check().is_err());
    for path in ["", "/absolute.json", "../outside.json"] {
        let mut declared = contract();
        declared.capture = Some(ResultCapture::new(path));
        assert!(declared.check().is_err());
    }
    let mut declared = contract();
    declared.capture.as_mut().unwrap().max_files = 0;
    assert!(declared.check().is_err());
    let value = serde_json::to_value(contract()).unwrap();
    assert_eq!(
        serde_json::from_value::<ResultCapture>(json!({"manifest":"outputs/artifacts.json"}))
            .unwrap(),
        ResultCapture::new("outputs/artifacts.json")
    );
    assert_eq!(value["capture"]["manifest"], "outputs/artifacts.json");
    assert_eq!(
        serde_json::from_value::<ResultContract>(value).unwrap(),
        contract()
    );
}

#[test]
fn a_managed_lead_parks_without_intermediate_output_then_captures_on_its_wake() {
    let f = Fixture::new();
    let final_prompt = script(
        &f,
        "wake-final",
        Some(manifest()),
        "printf %s 'checked evidence' > outputs/evidence.txt",
    );
    let wake = f.dir.join("wake.txt");
    fs::write(&wake, final_prompt).unwrap();
    let agent = common::fake_agent().display().to_string();
    let options = TaskOptions {
        command: Some(vec![
            "env".into(),
            format!("FAKE_ACP_WAKE={}", wake.display()),
            agent.clone(),
        ]),
        delegation: Some(branchyard::Envelope::default()),
        delegation_server: Some(vec![agent]),
        policy: branchyard::Policy::allow_all(),
        ..options(&f, contract())
    };
    let yard = f.yard.clone();
    let running_options = options.clone();
    let running = std::thread::spawn(move || {
        yard.task("AWAIT_STEER")
            .options(running_options)
            .name("lead")
            .run()
    });
    wait::until("the managed lead to wait for steering", || {
        f.yard.branch("lead").ok().and_then(|branch| {
            branch
                .events()
                .ok()
                .filter(|events| common::text(events).contains("waiting for steering"))
        })
    });
    let lead = f.yard.branch("lead").unwrap();
    let delegate = lead.delegate(options).unwrap();
    delegate
        .spawn(branchyard::Spawn {
            prompt: "AWAIT_STEER".into(),
            name: Some("worker".into()),
            ..branchyard::Spawn::default()
        })
        .unwrap();
    wait::until("the worker to remain active", || {
        delegate
            .inspect("worker")
            .ok()
            .filter(|inspection| inspection.last_message.contains("waiting for steering"))
    });
    lead.steer("End your own work while the worker continues")
        .unwrap();
    wait::until("the lead to park without a final outbox", || {
        f.yard
            .branch("lead")
            .ok()
            .filter(|branch| branch.info().status == BranchStatus::WaitingOnChildren)
    });
    let parked = f.yard.branch("lead").unwrap();
    assert!(!parked
        .info()
        .worktree
        .join("outputs/artifacts.json")
        .exists());
    assert!(f.yard.artifacts("lead").unwrap().is_empty());
    assert_eq!(
        branchyard::contract::current(&parked.events().unwrap()),
        None
    );
    delegate.steer("worker", "Finished").unwrap();
    wait::until(
        "the later lead turn to produce its captured final output",
        || {
            f.yard.branch("lead").ok().filter(|branch| {
                matches!(
                    branch.info().status,
                    BranchStatus::Ready | BranchStatus::NoChanges
                ) && branch.info().turns == 2
                    && branchyard::contract::current(&branch.events().unwrap())
                        == Some(json!({"answer":"checked"}))
                    && f.yard.artifacts("lead").unwrap().len() == 1
            })
        },
    );
    running.join().unwrap().unwrap();
    let finished = f.yard.branch("lead").unwrap();
    assert_eq!(
        branchyard::contract::current(&finished.events().unwrap()),
        Some(json!({"answer":"checked"}))
    );
    assert_eq!(f.yard.artifacts("lead").unwrap().len(), 1);
}
