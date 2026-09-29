//! Choosing where a branch's harness runs.

mod common;

use branchyard::{BranchStatus, Error, Provider, SandboxOptions, SubstrateOptions, TaskOptions};
use common::{stored_record, Fixture};

fn microsandbox() -> Provider {
    Provider::Microsandbox(SandboxOptions {
        image: "ghcr.io/example/harness:1".into(),
        cpus: Some(2),
        memory_mib: Some(2048),
        pass_env: vec!["ANTHROPIC_API_KEY".into()],
        ..SandboxOptions::default()
    })
}

#[test]
fn providers_serialize_tagged_by_kind() {
    let json = serde_json::to_value(microsandbox()).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "kind": "microsandbox",
            "image": "ghcr.io/example/harness:1",
            "cpus": 2,
            "memory_mib": 2048,
            "pass_env": ["ANTHROPIC_API_KEY"],
        })
    );
    assert_eq!(
        serde_json::from_value::<Provider>(json).unwrap(),
        microsandbox()
    );
    assert_eq!(
        serde_json::to_value(Provider::Local).unwrap(),
        serde_json::json!({"kind": "local"})
    );
}

#[test]
fn substrate_options_serialize_with_defaults_for_what_is_left_out() {
    let json = serde_json::json!({
        "kind": "substrate",
        "endpoint": "http://ate:8080",
        "router": "http://router/{atespace}/{actor}/",
        "template": "by",
        "key": "/keys/by.pk8",
    });
    let Provider::Substrate(options) = serde_json::from_value(json).unwrap() else {
        panic!("not substrate")
    };
    assert_eq!(options.atespace(), "default");
    assert_eq!(options.workdir(), branchyard::SANDBOX_WORKSPACE);
    assert_eq!(options.home(), branchyard::SANDBOX_HOME);
    let round = serde_json::to_value(Provider::Substrate(options.clone())).unwrap();
    assert_eq!(
        serde_json::from_value::<Provider>(round).unwrap(),
        Provider::Substrate(options)
    );
}

#[test]
fn a_substrate_provider_it_cannot_use_creates_nothing() {
    let f = Fixture::new();
    let refused = f
        .task("WRITE x.txt=1")
        .options(TaskOptions {
            provider: Some(Provider::Substrate(SubstrateOptions {
                endpoint: "http://127.0.0.1:9".into(),
                router: "http://127.0.0.1:9/{actor}/".into(),
                template: "by".into(),
                key: f.dir.join("no-such-key"),
                ..SubstrateOptions::default()
            })),
            ..f.options()
        })
        .name("keyless")
        .run();
    assert!(
        matches!(&refused, Err(Error::Unsupported(why)) if why.contains("bridge key")),
        "{refused:?}"
    );
    assert!(f.yard.branches().unwrap().is_empty());
}

#[test]
fn an_explicit_local_provider_runs_as_before_and_is_kept_for_sends() {
    let f = Fixture::new();
    let branch = f
        .task("WRITE local.txt=1")
        .name("local")
        .provider(Provider::Local)
        .run()
        .unwrap();
    assert_eq!(branch.info().status, BranchStatus::Ready);
    let record = stored_record(&f.root, "local");
    assert_eq!(record["provider"], serde_json::json!({"kind": "local"}));
    let sent = branch.send("WHOAMI", f.options()).unwrap();
    assert_eq!(sent.info().turns, 2);
}

#[test]
fn a_sandbox_this_build_cannot_run_creates_nothing() {
    if branchyard_microsandbox::ENABLED {
        return;
    }
    let f = Fixture::new();
    let refused = f
        .task("WRITE x.txt=1")
        .name("boxed")
        .provider(microsandbox())
        .run();
    assert!(
        matches!(&refused, Err(Error::Unsupported(why)) if why.contains("--features microsandbox")),
        "{refused:?}"
    );
    assert!(f.yard.branches().unwrap().is_empty());

    let empty = TaskOptions {
        provider: Some(Provider::Microsandbox(SandboxOptions::default())),
        ..f.options()
    };
    assert!(matches!(
        f.task("x").options(empty).run(),
        Err(Error::Unsupported(_))
    ));

    let parent = f.task("WRITE base.txt=1").name("parent").run().unwrap();
    let fork = parent.fork(
        "WRITE child.txt=2",
        true,
        TaskOptions {
            provider: Some(microsandbox()),
            ..f.options()
        },
    );
    assert!(matches!(fork, Err(Error::Unsupported(_))), "{fork:?}");
    assert_eq!(f.yard.branches().unwrap().len(), 1);
}
