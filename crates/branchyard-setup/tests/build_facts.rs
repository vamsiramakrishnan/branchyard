//! Local Microsandbox is offered only when this build and host can run it;
//! remote Substrate stays available, and a saved unavailable provider remains
//! selected with a warning. The server topic labels a PostgreSQL store a build
//! cannot open with the command that adds it. Hermetic: fake probes only.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
use std::collections::BTreeMap;

use branchyard_setup::probe::{BuildFacts, CountingEntropy, Facts, FakeProbe};
use branchyard_setup::{next, questions, BuiltinValidator, Plan, Question, Topic};
use serde_json::{json, Value};

/// The plan the defaults (and `answers`) lead to.
fn plan_for(topic: Topic, probe: &FakeProbe, answers: &BTreeMap<String, Value>) -> Plan {
    let response = next(
        topic,
        probe,
        answers,
        true,
        &mut CountingEntropy(0),
        &BuiltinValidator,
    );
    assert!(response.done, "{response:?}");
    response.plan.unwrap()
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

    // No SDK: local and remote actors remain available, but a new local
    // Microsandbox choice is absent even with KVM. The plan explains the build.
    let probe = machine(BuildFacts {
        microsandbox: false,
        postgres: true,
    });
    let all = asked(Topic::Project, &probe);
    let provider = find(&all, "provider").unwrap();
    assert_eq!(provider.default, json!("local"));
    assert_eq!(
        provider
            .choices
            .iter()
            .map(|choice| choice.value.clone())
            .collect::<Vec<_>>(),
        [json!("local"), json!("substrate")]
    );
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
    let packaged = note.find(branchyard_setup::probe::INSTALL_RUNTIME).unwrap();
    let source = note
        .find(branchyard_setup::probe::REBUILD_MICROSANDBOX)
        .unwrap();
    assert!(packaged < source, "lead with the published package: {note}");
    assert!(note.contains("docs/distribution.md"), "{note}");

    // Without KVM, a new local microVM is unavailable even in a runtime build.
    // Neither build hides remote Substrate, which uses its cluster's runtime.
    for build in [NONE, BuildFacts::full()] {
        let mut probe = machine(build);
        probe.platform.kvm = false;
        let all = asked(Topic::Project, &probe);
        let provider = find(&all, "provider").unwrap();
        assert_eq!(
            provider
                .choices
                .iter()
                .map(|choice| choice.value.clone())
                .collect::<Vec<_>>(),
            [json!("local"), json!("substrate")]
        );
        let plan = plan_for(Topic::Project, &probe, &BTreeMap::new());
        assert!(
            !plan.notes.iter().any(|n| n.contains("Microsandbox")),
            "{:?}",
            plan.notes
        );
    }
}

#[test]
fn a_file_that_names_microsandbox_keeps_the_choice_and_says_what_is_missing() {
    let mut probe = machine(NONE);
    probe.platform.kvm = false;
    probe.files.insert(
        "branchyard.toml".into(),
        "version = 1\n[defaults]\nprovider = \"microsandbox\"\n[microsandbox]\n\
         image = \"alpine:3.20\"\ncpus = 3\nmemory_mib = 8192\n\
         pass_env = [\"LANG\"]\nkeep = \"pause\"\nsnapshots = 2\nmax_paused = 4\nmax_kept_minutes = 10\n"
            .into(),
    );
    let all = asked(Topic::Project, &probe);
    let provider = find(&all, "provider").unwrap();
    assert_eq!(provider.default, json!("microsandbox"));
    assert!(provider.why.contains("Remote actors"), "{}", provider.why);
    assert!(
        provider.why.contains("host login does not transfer"),
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
        "needs the Linux runtime package (install.sh --runtime; docs/distribution.md)"
    );
    let before = branchyard_setup::config::parse(&probe.files["branchyard.toml"]).unwrap();
    let plan = plan_for(Topic::Project, &probe, &BTreeMap::new());
    assert!(
        plan.notes
            .iter()
            .any(|note| note.contains(branchyard_setup::probe::INSTALL_RUNTIME)),
        "{:?}",
        plan.notes
    );
    let after = branchyard_setup::config::parse(plan.files[0].content.as_deref().unwrap()).unwrap();
    assert_eq!(after.defaults.provider, before.defaults.provider);
    assert_eq!(
        after.microsandbox, before.microsandbox,
        "preserve unasked runtime settings"
    );
}

#[test]
fn a_saved_local_microsandbox_warns_when_the_runtime_build_has_no_kvm() {
    let mut probe = machine(BuildFacts::full());
    probe.platform.kvm = false;
    probe.files.insert(
        "branchyard.toml".into(),
        "[defaults]\nprovider = \"microsandbox\"\n[microsandbox]\nimage = \"alpine:3.20\"\n".into(),
    );
    let all = asked(Topic::Project, &probe);
    let provider = find(&all, "provider").unwrap();
    assert_eq!(provider.default, json!("microsandbox"));
    let choice = provider
        .choices
        .iter()
        .find(|choice| choice.value == "microsandbox")
        .unwrap();
    assert_eq!(choice.description, "needs a Linux host with KVM");
}

#[test]
fn remote_microsandbox_describes_the_servers_requirements_not_the_clients() {
    let probe = machine(NONE);
    let answers = BTreeMap::from([("remote".into(), json!("server"))]);
    let all = questions(Topic::Project, &Facts::gather(&probe), &answers);
    let choice = find(&all, "provider")
        .unwrap()
        .choices
        .iter()
        .find(|choice| choice.value == "microsandbox")
        .unwrap();
    assert_eq!(
        choice.description,
        "microVMs on the server; the server needs Microsandbox and KVM"
    );
}

#[test]
fn a_substrate_plan_does_not_claim_harnesses_run_locally_without_microsandbox() {
    let probe = machine(NONE);
    let answers = BTreeMap::from([
        ("provider".into(), json!("substrate")),
        (
            "substrate.endpoint".into(),
            json!("https://control.example"),
        ),
        ("substrate.router".into(), json!("https://router.example")),
        ("substrate.atespace".into(), json!("default")),
        ("substrate.template".into(), json!("agy")),
        ("substrate.key".into(), json!("bridge.key")),
    ]);
    let plan = plan_for(Topic::Project, &probe, &answers);
    let body = plan.files[0].content.as_deref().unwrap();
    assert_eq!(
        branchyard_setup::config::parse(body)
            .unwrap()
            .defaults
            .provider,
        Some(branchyard_setup::config::ProviderKind::Substrate)
    );
    let note = plan
        .notes
        .iter()
        .find(|note| note.contains("no Microsandbox support"))
        .unwrap();
    assert!(note.contains("local microVMs cannot run here"), "{note}");
    assert!(!note.contains("harnesses run locally"), "{note}");
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
