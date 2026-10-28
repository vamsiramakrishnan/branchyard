//! Hermetic authentication, template reconciliation and per-actor policy checks.
mod common;

use branchyard_bridge::Signer;
use branchyard_sandbox::{SandboxProvider, SandboxSpec};
use branchyard_substrate::fake::FakeCluster;
use branchyard_substrate::template::{
    bridge_template_with, BridgeOptions, BridgeTemplate, TemplateResources,
};
use branchyard_substrate::{pb, Config, EgressPolicy, SetupAction, SubstrateProvider};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

fn spec(signer: &Signer) -> BridgeTemplate {
    BridgeTemplate {
        name: "bridge".into(),
        image: format!("registry.example/bridge@sha256:{}", "a".repeat(64)),
        bridge: "/usr/local/bin/branchyard-bridge".into(),
        public_key: signer.public_key(),
        sandbox_class: pb::SandboxClass::Gvisor,
        sandbox_config: "gvisor".into(),
        storage_location: "gs://snapshots/branchyard".into(),
    }
}

#[test]
fn setup_rechecks_and_reuses_matching_immutable_resources() -> Result {
    let scratch = tempfile::tempdir()?;
    let fake = FakeCluster::start("actors", None, scratch.path());
    let (signer, _) = Signer::generate()?;
    let spec = spec(&signer);
    let options = BridgeOptions {
        worker_selector: [("workload".into(), "branchyard".into())].into(),
        resources: TemplateResources {
            cpu: Some("2".into()),
            memory: Some("4Gi".into()),
        },
        run_as: Some((1000, 1000)),
        ..Default::default()
    };
    let provider = SubstrateProvider::connect(
        Config::new(fake.endpoint(), "actors", "templates/bridge", fake.router()).signer(signer),
    )?;
    let plan = provider.plan_setup(&spec, &options)?;
    assert_eq!(
        plan.actions,
        vec![
            SetupAction::CreateAtespace {
                name: "templates".into()
            },
            SetupAction::CreateTemplate
        ]
    );
    let result = provider.apply_setup(&plan, |_| {})?;
    assert!(result.created_atespace && result.created_template);
    let inspection = provider.inspect_template()?;
    assert_eq!(inspection.atespace, "templates");
    assert_eq!(inspection.bridge_port, 80);
    assert!(provider.plan_setup(&spec, &options)?.actions.is_empty());
    let again = provider.apply_setup(&plan, |_| {})?;
    assert!(!again.created_atespace && !again.created_template);
    let mut different = spec.clone();
    different.image = format!("registry.example/bridge@sha256:{}", "b".repeat(64));
    assert!(provider.plan_setup(&different, &options).is_err());
    let mut resized = options.clone();
    resized.resources.memory = Some("8Gi".into());
    assert!(provider.plan_setup(&spec, &resized).is_err());
    let mut missing_capabilities = bridge_template_with("templates", &spec, &options);
    missing_capabilities.containers[0].security_context = None;
    fake.add_template(missing_capabilities);
    assert!(provider
        .inspect_template()
        .err()
        .ok_or("missing capabilities accepted")?
        .to_string()
        .contains("SETUID and SETGID"));
    Ok(())
}

#[test]
fn setup_refuses_a_conflict_introduced_after_the_plan() -> Result {
    let scratch = tempfile::tempdir()?;
    let fake = FakeCluster::start("actors", None, scratch.path());
    let (signer, _) = Signer::generate()?;
    let mut spec = spec(&signer);
    let provider = SubstrateProvider::connect(Config::new(
        fake.endpoint(),
        "actors",
        "bridge",
        fake.router(),
    ))?;
    let plan = provider.plan_setup(&spec, &BridgeOptions::default())?;
    spec.public_key = "ab".repeat(32);
    fake.add_template(bridge_template_with(
        "actors",
        &spec,
        &BridgeOptions::default(),
    ));
    assert!(provider.apply_setup(&plan, |_| {}).is_err());
    assert!(fake.actor_names().is_empty());
    Ok(())
}

#[test]
fn api_token_is_reloaded_for_setup_inspection_and_lifecycle() -> Result {
    let scratch = tempfile::tempdir()?;
    let fake = FakeCluster::start(
        "actors",
        Some(common::bridge_binary().to_path_buf()),
        &scratch.path().join("fake"),
    );
    fake.require_api_token("first");
    let (signer, _) = Signer::generate()?;
    let spec = spec(&signer);
    let token_file = scratch.path().join("token");
    std::fs::write(&token_file, "first\n")?;
    let mut config = Config::new(fake.endpoint(), "actors", "bridge", fake.router());
    config.api_token_file = Some(token_file.clone());
    let provider = SubstrateProvider::connect(config.clone())?;
    let plan = provider.plan_setup(&spec, &BridgeOptions::default())?;
    provider.apply_setup(&plan, |_| {})?;
    assert!(provider.inspect_template()?.exec);
    fake.require_api_token("second");
    assert!(provider.inspect_template().is_err());
    std::fs::write(&token_file, "second\n")?;
    assert!(provider.inspect_template()?.exec);
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let mut actors = config.actors()?;
        let actor = actors.ensure("root", "bridge").await?;
        fake.require_api_token("third");
        std::fs::write(&token_file, "third")?;
        actors.destroy(&actor).await?;
        Result::Ok(())
    })?;
    std::fs::write(&token_file, "sensitive\nmalformed")?;
    let error = provider
        .inspect_template()
        .err()
        .ok_or("malformed token accepted")?
        .to_string();
    assert!(!error.contains("sensitive"));
    assert!(error.contains("one nonempty token"));
    std::fs::remove_file(&token_file)?;
    assert!(provider.inspect_template().is_err());
    Ok(())
}

#[test]
fn actors_compare_full_template_identity_and_create_child_policies() -> Result {
    let scratch = tempfile::tempdir()?;
    let fake = FakeCluster::start(
        "actors",
        Some(common::bridge_binary().to_path_buf()),
        scratch.path(),
    );
    let (signer, _) = Signer::generate()?;
    let spec = spec(&signer);
    for atespace in ["actors", "templates"] {
        fake.add_template(bridge_template_with(
            atespace,
            &spec,
            &BridgeOptions::default(),
        ));
    }
    let mut config = Config::new(fake.endpoint(), "actors", "templates/bridge", fake.router());
    config.egress = EgressPolicy::AllowHosts(vec!["api.example.com".into()]);
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let mut actors = config.actors()?;
        let root = actors.ensure("root", "templates/bridge").await?;
        assert!(actors.ensure("root", "bridge").await.is_err());
        actors.stop(&root).await?;
        let checkpoint = actors.checkpoint(&root, "checkpoint").await?;
        let child = actors
            .branch(&checkpoint, "child", "templates/bridge")
            .await?;
        let root_policy = fake.egress_policy("root").ok_or("root policy missing")?;
        let child_policy = fake.egress_policy("child").ok_or("child policy missing")?;
        assert_eq!(root_policy.rules, child_policy.rules);
        config.egress = EgressPolicy::Unrestricted;
        assert!(config
            .actors()?
            .ensure("root", "templates/bridge")
            .await
            .is_err());
        actors.destroy(&root).await?;
        actors.destroy(&child).await?;
        assert!(fake.egress_policy("root").is_none());
        Result::Ok(())
    })?;
    Ok(())
}

#[test]
fn setup_rejects_unpinned_images_and_wrong_signing_keys() -> Result {
    let scratch = tempfile::tempdir()?;
    let fake = FakeCluster::start("actors", None, scratch.path());
    let (signer, _) = Signer::generate()?;
    let mut spec = spec(&signer);
    let (other, _) = Signer::generate()?;
    let provider = SubstrateProvider::connect(
        Config::new(fake.endpoint(), "actors", "bridge", fake.router()).signer(other),
    )?;
    fake.add_template(bridge_template_with(
        "actors",
        &spec,
        &BridgeOptions::default(),
    ));
    assert!(provider.inspect_template().is_err());
    assert!(provider
        .plan_setup(&spec, &BridgeOptions::default())
        .is_err());
    spec.image = "registry.example/bridge:latest".into();
    assert!(provider
        .plan_setup(&spec, &BridgeOptions::default())
        .is_err());
    assert!(provider
        .ensure(&SandboxSpec::new("bad").image("other"))
        .is_err());
    Ok(())
}

#[test]
fn a_qualified_template_does_not_hide_an_invalid_actor_atespace() {
    let config = Config::new(
        "http://127.0.0.1:9",
        "bad/atespace",
        "templates/bridge",
        "http://127.0.0.1:9",
    );
    assert!(config.check().is_err());
}
