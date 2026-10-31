//! Hermetic checks of the read-only `Control` probes against the in-process
//! fake: which stage a failure is attributed to, and that probing provisions
//! nothing. Not evidence about a Substrate cluster.

use std::path::PathBuf;

use branchyard_bridge::Signer;
use branchyard_substrate::fake::FakeCluster;
use branchyard_substrate::template::{bridge_template_with, BridgeOptions, BridgeTemplate};
use branchyard_substrate::{pb, Config, ControlError, SubstrateProvider, WorkerCapacity};
use branchyard_testkit::Scratch;

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

const ATESPACE: &str = "actors";
const REFUSED: &str = "http://127.0.0.1:1";

/// A fake whose template `bridge` runs the bridge with `signer`'s key, and
/// whose template `plain` runs nothing, so an actor needs no bridge binary.
fn cluster(scratch: &Scratch, signer: &Signer) -> FakeCluster {
    let fake = FakeCluster::start(ATESPACE, None, &scratch.join("fake"));
    let spec = BridgeTemplate {
        name: "bridge".into(),
        image: format!("registry.example/bridge@sha256:{}", "a".repeat(64)),
        bridge: "/usr/local/bin/branchyard-bridge".into(),
        public_key: signer.public_key(),
        sandbox_class: pb::SandboxClass::Gvisor,
        sandbox_config: "gvisor".into(),
        storage_location: "gs://snapshots/branchyard".into(),
    };
    fake.add_template(bridge_template_with(
        ATESPACE,
        &spec,
        &BridgeOptions::default(),
    ));
    fake.add_template(pb::ActorTemplate {
        metadata: Some(pb::ResourceMetadata {
            atespace: ATESPACE.into(),
            name: "plain".into(),
            ..Default::default()
        }),
        ..Default::default()
    });
    fake
}

fn config(endpoint: &str, template: &str) -> Config {
    Config::new(endpoint, ATESPACE, template, format!("{endpoint}/"))
}

fn with_token(mut config: Config, token_file: PathBuf) -> Config {
    config.api_token_file = Some(token_file);
    config
}

fn worker(state: pb::WorkerState, capacity: Option<i32>, allocated: Option<i32>) -> pb::Worker {
    let resources = |actors: Option<i32>| {
        actors.map(|actors| pb::WorkerResources {
            actors,
            ..Default::default()
        })
    };
    pb::Worker {
        status: Some(pb::WorkerStatus {
            state: state as i32,
            capacity: resources(capacity),
            allocated: resources(allocated),
        }),
        ..Default::default()
    }
}

#[test]
fn probe_template_inspects_a_valid_template_signed_by_the_configured_key() -> Result {
    let scratch = Scratch::new("probe-valid");
    let (signer, _) = Signer::generate()?;
    let fake = cluster(&scratch, &signer);
    let public_key = signer.public_key();
    let provider = SubstrateProvider::connect(config(fake.endpoint(), "bridge").signer(signer))?;
    let inspection = provider.probe_template()?;
    assert!(inspection.exec);
    assert_eq!(inspection.public_key, public_key);
    Ok(())
}

#[test]
fn probe_template_is_unreachable_when_nothing_listens() -> Result {
    let provider = SubstrateProvider::connect(config(REFUSED, "bridge"))?;
    let result = provider.probe_template();
    assert!(
        matches!(result, Err(ControlError::Unreachable(_))),
        "{result:?}"
    );
    Ok(())
}

#[test]
fn probe_template_is_unreachable_when_the_endpoint_is_not_grpc() -> Result {
    let scratch = Scratch::new("probe-not-grpc");
    let (signer, _) = Signer::generate()?;
    let fake = cluster(&scratch, &signer);
    let router = fake.router().trim_end_matches('/');
    let result = SubstrateProvider::connect(config(router, "bridge"))?.probe_template();
    assert!(
        matches!(result, Err(ControlError::Unreachable(_))),
        "{result:?}"
    );
    Ok(())
}

#[test]
fn probe_template_is_rejected_without_the_right_token() -> Result {
    let scratch = Scratch::new("probe-rejected");
    let (signer, _) = Signer::generate()?;
    let fake = cluster(&scratch, &signer);
    fake.require_api_token("right");
    let wrong = scratch.join("wrong-token");
    std::fs::write(&wrong, "wrong\n")?;
    for config in [
        config(fake.endpoint(), "bridge"),
        with_token(config(fake.endpoint(), "bridge"), wrong),
        with_token(config(fake.endpoint(), "bridge"), scratch.join("absent")),
    ] {
        let token_file = config.api_token_file.clone();
        let result = SubstrateProvider::connect(config)?.probe_template();
        assert!(
            matches!(result, Err(ControlError::Rejected(_))),
            "token file {token_file:?}: {result:?}"
        );
    }
    Ok(())
}

#[test]
fn probe_template_reports_an_absent_template_as_answered() -> Result {
    let scratch = Scratch::new("probe-absent");
    let (signer, _) = Signer::generate()?;
    let fake = cluster(&scratch, &signer);
    let result = SubstrateProvider::connect(config(fake.endpoint(), "absent"))?.probe_template();
    assert!(
        matches!(result, Err(ControlError::Answered(_))),
        "{result:?}"
    );
    Ok(())
}

#[test]
fn probe_template_reports_a_template_for_another_signer() -> Result {
    let scratch = Scratch::new("probe-signer");
    let (signer, _) = Signer::generate()?;
    let fake = cluster(&scratch, &signer);
    let (other, _) = Signer::generate()?;
    let result = SubstrateProvider::connect(config(fake.endpoint(), "bridge").signer(other))?
        .probe_template();
    assert!(
        matches!(result, Err(ControlError::SignerMismatch(_))),
        "{result:?}"
    );
    Ok(())
}

#[test]
fn worker_capacity_counts_free_slots_on_active_reporting_workers() -> Result {
    let scratch = Scratch::new("probe-workers");
    let (signer, _) = Signer::generate()?;
    let fake = cluster(&scratch, &signer);
    let provider = SubstrateProvider::connect(config(fake.endpoint(), "bridge"))?;
    assert_eq!(
        provider.worker_capacity()?,
        WorkerCapacity {
            listed: 0,
            active: 0,
            reporting: 0,
            free_actor_slots: 0,
            more: false,
        }
    );
    fake.set_workers(vec![
        worker(pb::WorkerState::Active, Some(4), Some(1)),
        worker(pb::WorkerState::Active, Some(2), Some(2)),
        worker(pb::WorkerState::Draining, Some(8), None),
    ]);
    assert_eq!(
        provider.worker_capacity()?,
        WorkerCapacity {
            listed: 3,
            active: 2,
            reporting: 2,
            free_actor_slots: 3,
            more: false,
        }
    );
    fake.set_workers(vec![worker(pb::WorkerState::Active, None, None)]);
    assert_eq!(
        provider.worker_capacity()?,
        WorkerCapacity {
            listed: 1,
            active: 1,
            reporting: 0,
            free_actor_slots: 0,
            more: false,
        }
    );
    Ok(())
}

#[test]
fn worker_capacity_names_a_wrong_token_and_an_unreachable_control() -> Result {
    let scratch = Scratch::new("probe-workers-fail");
    let (signer, _) = Signer::generate()?;
    let fake = cluster(&scratch, &signer);
    fake.require_api_token("right");
    let wrong = scratch.join("wrong-token");
    std::fs::write(&wrong, "wrong\n")?;
    let rejected =
        SubstrateProvider::connect(with_token(config(fake.endpoint(), "bridge"), wrong))?
            .worker_capacity();
    assert!(
        matches!(rejected, Err(ControlError::Rejected(_))),
        "{rejected:?}"
    );
    let unreachable = SubstrateProvider::connect(config(REFUSED, "bridge"))?.worker_capacity();
    assert!(
        matches!(unreachable, Err(ControlError::Unreachable(_))),
        "{unreachable:?}"
    );
    Ok(())
}

#[test]
fn probes_create_no_actor() -> Result {
    let scratch = Scratch::new("probe-creates-nothing");
    let (signer, _) = Signer::generate()?;
    let fake = cluster(&scratch, &signer);
    let config = config(fake.endpoint(), "bridge");
    let provider = SubstrateProvider::connect(config.clone())?;
    provider.probe_template()?;
    provider.worker_capacity()?;
    assert_eq!(fake.created_actors(), 0);
    tokio::runtime::Runtime::new()?.block_on(async {
        config.actors()?.ensure("root", "plain").await?;
        Result::Ok(())
    })?;
    assert_eq!(fake.created_actors(), 1);
    Ok(())
}
