//! What a branch's record says about where its harness ran
//! (`BranchInfo::provider`, `BranchInfo::isolated`), so a reviewer can tell
//! a plain local run from an isolated one afterwards; and that a record
//! written before the fields existed still loads. Hermetic: the fake ACP
//! agent.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use branchyard::{BranchInfo, Provider, RecipeOptions, SandboxOptions, SubstrateOptions};
use common::{edit_record, stored_record, Fixture};

#[test]
fn a_plain_run_is_recorded_local_and_an_isolated_one_says_so() {
    let f = Fixture::new();
    let plain = f.task("WRITE a.txt=1").name("plain").run().unwrap();
    assert_eq!(plain.info().provider, "local");
    assert!(!plain.info().isolated);

    let isolated = f
        .task("WRITE b.txt=1")
        .name("private")
        .isolated(true)
        .run()
        .unwrap();
    assert_eq!(isolated.info().provider, "local");
    assert!(isolated.info().isolated, "a private HOME is recorded");

    // What `by ls --json` and a server print is the record itself.
    let value = serde_json::to_value(isolated.info()).unwrap();
    assert_eq!(value["provider"], "local");
    assert_eq!(value["isolated"], true);

    // A fork of an isolated branch is isolated too, as the engine runs it.
    let fork = isolated
        .fork(
            "WRITE c.txt=1",
            true,
            branchyard::TaskOptions {
                name: Some("fork".into()),
                ..f.options()
            },
        )
        .unwrap();
    assert!(fork.info().isolated);
    assert!(!f.yard.branch("plain").unwrap().info().isolated);
}

#[test]
fn a_record_from_before_the_fields_reads_local_and_is_filled_by_its_next_turn() {
    let f = Fixture::new();
    let branch = f
        .task("WRITE a.txt=1")
        .name("old")
        .isolated(true)
        .run()
        .unwrap();
    // As an older `by` wrote it: neither key.
    edit_record(&f.root, "old", |record| {
        let info = record["info"].as_object_mut().unwrap();
        assert!(info.remove("provider").is_some());
        assert!(info.remove("isolated").is_some());
    });
    assert!(stored_record(&f.root, "old")["info"]
        .get("provider")
        .is_none());
    let info = f.yard.branch("old").unwrap().info().clone();
    assert_eq!(info.provider, "local");
    assert!(
        !info.isolated,
        "the old record does not say; it reads false"
    );

    // The next turn records what it ran as.
    let sent = branch.send("WRITE b.txt=1", f.options()).unwrap();
    assert!(sent.info().isolated, "its home is private");
    assert_eq!(sent.info().provider, "local");
}

#[test]
fn branch_info_json_without_the_fields_still_loads() {
    let f = Fixture::new();
    let branch = f.task("WRITE a.txt=1").name("json").run().unwrap();
    let mut value = serde_json::to_value(branch.info()).unwrap();
    let keys = value.as_object_mut().unwrap();
    keys.remove("provider");
    keys.remove("isolated");
    let info: BranchInfo = serde_json::from_value(value).unwrap();
    assert_eq!(info.provider, "local");
    assert!(!info.isolated);
}

#[test]
fn every_provider_is_named_as_the_flag_names_it() {
    assert_eq!(Provider::Local.label(), "local");
    assert_eq!(
        Provider::Microsandbox(SandboxOptions::default()).label(),
        "microsandbox"
    );
    assert_eq!(
        Provider::Substrate(SubstrateOptions::default()).label(),
        "substrate"
    );
    let recipe = RecipeOptions {
        name: "devbox".into(),
        ..RecipeOptions::default()
    };
    assert_eq!(Provider::Recipe(recipe).label(), "recipe:devbox");
}
