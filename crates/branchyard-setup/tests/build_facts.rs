//! The interview never offers what the build running it cannot serve: the
//! provider question is asked only where Microsandbox can run, and the
//! server topic labels a PostgreSQL store a build cannot open with the
//! command that adds it. Over a fake probe; no machine is read.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
use std::collections::BTreeMap;

use branchyard_setup::probe::{BuildFacts, CountingEntropy, Facts, FakeProbe};
use branchyard_setup::{next, questions, BuiltinValidator, Plan, Question, Topic};
use serde_json::{json, Value};

/// The plan the defaults (and `answers`) lead to.
fn plan_for(topic: Topic, probe: &FakeProbe, answers: &BTreeMap<String, Value>) -> Plan {
    next(
        topic,
        probe,
        answers,
        true,
        &mut CountingEntropy(0),
        &BuiltinValidator,
    )
    .plan
    .unwrap()
}

/// Every question `topic` may ask on `probe`'s machine.
fn asked(topic: Topic, probe: &FakeProbe) -> Vec<Question> {
    questions(topic, &Facts::gather(probe), &BTreeMap::new())
}

fn find<'a>(all: &'a [Question], id: &str) -> Option<&'a Question> {
    all.iter().find(|q| q.id == id)
}

/// A machine with KVM, from a build with only the features given.
fn machine(build: BuildFacts) -> FakeProbe {
    let mut probe = FakeProbe::typical();
    probe.platform.kvm = true;
    probe.build = build;
    probe
}

const NONE: BuildFacts = BuildFacts {
    microsandbox: false,
    postgres: false,
};

#[test]
fn the_facts_say_what_the_build_can_serve() {
    let facts = |build| serde_json::to_value(Facts::gather(&machine(build))).unwrap();
    assert_eq!(
        facts(NONE)["build"],
        json!({ "microsandbox": false, "postgres": false })
    );
    assert_eq!(
        facts(BuildFacts::full())["build"],
        json!({ "microsandbox": true, "postgres": true })
    );
}

#[test]
fn a_kvm_host_is_offered_microsandbox_only_by_a_build_that_can_run_it() {
    // A build with the SDK, on a host with KVM: asked, and Microsandbox is
    // a plain choice.
    let all = asked(Topic::Project, &machine(BuildFacts::full()));
    let provider = find(&all, "provider").unwrap();
    let choice = provider
        .choices
        .iter()
        .find(|c| c.value == "microsandbox")
        .unwrap();
    assert!(!choice.description.contains("needs a build"), "{choice:?}");

    // No SDK: not asked, though KVM is there, and the plan says why it is not
    // and what to run to get it.
    let probe = machine(BuildFacts {
        microsandbox: false,
        postgres: true,
    });
    assert!(find(&asked(Topic::Project, &probe), "provider").is_none());
    let plan = plan_for(Topic::Project, &probe, &BTreeMap::new());
    let note = plan
        .notes
        .iter()
        .find(|n| n.contains("no Microsandbox support"))
        .unwrap_or_else(|| panic!("a note about the build in {:?}", plan.notes));
    assert!(
        note.contains(
            "cargo install --locked --path crates/branchyard-cli --features microsandbox"
        ),
        "{note}"
    );
    assert!(note.contains("libcap-ng"), "{note}");

    // Without KVM there is nothing to say either way.
    let mut probe = machine(NONE);
    probe.platform.kvm = false;
    assert!(find(&asked(Topic::Project, &probe), "provider").is_none());
    let plan = plan_for(Topic::Project, &probe, &BTreeMap::new());
    assert!(
        !plan.notes.iter().any(|n| n.contains("Microsandbox")),
        "{:?}",
        plan.notes
    );
}

#[test]
fn a_file_that_names_microsandbox_keeps_the_choice_and_says_what_is_missing() {
    let mut probe = machine(NONE);
    probe.platform.kvm = false;
    probe.files.insert(
        "branchyard.toml".into(),
        "version = 1\n[defaults]\nprovider = \"microsandbox\"\n[microsandbox]\nimage = \"alpine:3.20\"\n"
            .into(),
    );
    let all = asked(Topic::Project, &probe);
    let provider = find(&all, "provider").unwrap();
    assert_eq!(provider.default, json!("microsandbox"));
    assert!(
        provider.why.contains(
            "cargo install --locked --path crates/branchyard-cli --features microsandbox"
        ),
        "{}",
        provider.why
    );
    let choice = provider
        .choices
        .iter()
        .find(|c| c.value == "microsandbox")
        .unwrap();
    assert_eq!(
        choice.description,
        "needs a build with --features microsandbox"
    );
}

#[test]
fn the_server_topic_labels_what_the_build_cannot_open() {
    let all = asked(Topic::Server, &machine(BuildFacts::full()));
    let database = find(&all, "database").unwrap();
    let postgres = database
        .choices
        .iter()
        .find(|c| c.value == "postgres")
        .unwrap();
    assert!(
        !postgres.description.contains("needs a build"),
        "{postgres:?}"
    );
    assert!(!database.why.contains("cannot open it"), "{}", database.why);

    let all = asked(Topic::Server, &machine(NONE));
    let database = find(&all, "database").unwrap();
    let postgres = database
        .choices
        .iter()
        .find(|c| c.value == "postgres")
        .unwrap();
    assert!(
        postgres
            .description
            .starts_with("needs a build with --features postgres"),
        "{postgres:?}"
    );
    assert!(
        database
            .why
            .contains("cargo install --locked --path crates/branchyard-cli --features postgres"),
        "{}",
        database.why
    );
    let providers = find(&all, "allow_providers").unwrap();
    let microsandbox = providers
        .choices
        .iter()
        .find(|c| c.value == "microsandbox")
        .unwrap();
    assert_eq!(
        microsandbox.description,
        "microVMs; needs a build with --features microsandbox"
    );
}

#[test]
fn a_postgres_server_planned_on_a_build_without_it_says_so() {
    let answers: BTreeMap<String, Value> = [
        ("database".to_owned(), json!("postgres")),
        (
            "database.url".to_owned(),
            json!("postgres://branchyard@localhost/branchyard"),
        ),
    ]
    .into();
    let without = plan_for(Topic::Server, &machine(NONE), &answers);
    let note = without
        .notes
        .iter()
        .find(|n| n.contains("cannot open PostgreSQL"))
        .unwrap_or_else(|| panic!("a note about the build in {:?}", without.notes));
    assert!(note.contains("--features postgres"), "{note}");

    // On a build that can, no such note; and every plan names the token.
    let with = plan_for(Topic::Server, &machine(BuildFacts::full()), &answers);
    assert!(!with
        .notes
        .iter()
        .any(|n| n.contains("cannot open PostgreSQL")));
    assert!(
        with.notes
            .iter()
            .any(|n| n.contains("--token-file") && n.contains(".branchyard/server/token")),
        "{:?}",
        with.notes
    );
}
