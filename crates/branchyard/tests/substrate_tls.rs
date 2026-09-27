//! A turn in an Agent Substrate actor with TLS on both hops: the engine's
//! `SubstrateOptions` carry the authority and the client certificate to
//! the provider, against the fake cluster serving TLS with certificates
//! generated here. Hermetic; not evidence about a Substrate cluster.

mod common;

use std::fs;
use std::path::Path;

use branchyard::{BranchStatus, Policy, Provider, SubstrateOptions, TaskOptions};
use branchyard_bridge::Signer;
use branchyard_substrate::fake::{FakeCluster, FakeTls};
use branchyard_substrate::pb;
use branchyard_substrate::template::{bridge_template, BridgeTemplate};
use common::{bridge_binary, git, Fixture};

const ATESPACE: &str = "yard";
const TEMPLATE: &str = "by-harness";

fn authority(path: &Path) -> rcgen::Issuer<'static, rcgen::KeyPair> {
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(Vec::new()).unwrap();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let cert = params.self_signed(&key).unwrap();
    fs::write(path, cert.pem()).unwrap();
    rcgen::Issuer::new(params, key)
}

fn leaf(ca: &rcgen::Issuer<'_, rcgen::KeyPair>, dir: &Path, stem: &str) -> (String, String) {
    let key = rcgen::KeyPair::generate().unwrap();
    let params =
        rcgen::CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()]).unwrap();
    let cert = params.signed_by(&key, ca).unwrap();
    let (cert_path, key_path) = (
        dir.join(format!("{stem}.crt")),
        dir.join(format!("{stem}.key")),
    );
    fs::write(&cert_path, cert.pem()).unwrap();
    fs::write(&key_path, key.serialize_pem()).unwrap();
    (
        cert_path.display().to_string(),
        key_path.display().to_string(),
    )
}

#[test]
fn a_turn_runs_in_an_actor_over_tls_with_a_client_certificate() {
    let f = Fixture::new();
    let pki = f.dir.join("pki");
    fs::create_dir_all(&pki).unwrap();
    let ca = authority(&pki.join("ca.pem"));
    let (cert, key) = leaf(&ca, &pki, "server");
    let (client_cert, client_key) = leaf(&ca, &pki, "client");
    let fake = FakeCluster::start_tls(
        ATESPACE,
        Some(bridge_binary().to_path_buf()),
        &f.dir.join("cluster"),
        FakeTls {
            ca: pki.join("ca.pem"),
            cert: cert.into(),
            key: key.into(),
            client_ca: Some(pki.join("ca.pem")),
        },
    );
    let signing = f.dir.join("bridge.key");
    let signer = Signer::write(&signing).unwrap();
    fake.add_template(bridge_template(
        ATESPACE,
        &BridgeTemplate {
            name: TEMPLATE.into(),
            image: "registry.example/by@sha256:00".into(),
            bridge: "/usr/local/bin/branchyard-bridge".into(),
            public_key: signer.public_key(),
            sandbox_class: pb::SandboxClass::Gvisor,
            sandbox_config: "gvisor".into(),
            storage_location: "gs://bucket/by".into(),
        },
    ));
    let workdir = f.dir.join("actor/workspace");
    let home = f.dir.join("actor/home");
    fake.fresh_on_create(&workdir);
    fake.fresh_on_create(&home);
    let substrate = SubstrateOptions {
        endpoint: fake.endpoint().into(),
        router: fake.router().into(),
        atespace: ATESPACE.into(),
        template: TEMPLATE.into(),
        key: signing,
        workdir: workdir.display().to_string(),
        home: home.display().to_string(),
        ca: Some(pki.join("ca.pem")),
        client_cert: Some(client_cert.into()),
        client_key: Some(client_key.into()),
        ..SubstrateOptions::default()
    };
    assert!(substrate.endpoint.starts_with("https://"));
    let options = TaskOptions {
        provider: Some(Provider::Substrate(substrate.clone())),
        ..f.options()
    };
    let branch = f
        .task("WRITE secure.txt=over-tls")
        .options(options)
        .name("secure")
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    let info = branch.info();
    assert_eq!(info.status, BranchStatus::Ready, "{:?}", branch.events());
    let candidate = info.candidate.as_ref().unwrap();
    assert_eq!(
        git(
            &f.root,
            &["show", &format!("{}:secure.txt", candidate.commit)]
        ),
        "over-tls\n"
    );
    assert!(fake.actor_names().is_empty());

    // Without the client certificate the turn cannot even start an actor.
    let anonymous = SubstrateOptions {
        client_cert: None,
        client_key: None,
        ..substrate
    };
    let refused = f
        .task("WRITE never.txt=x")
        .options(TaskOptions {
            provider: Some(Provider::Substrate(anonymous)),
            ..f.options()
        })
        .name("anonymous")
        .policy(Policy::allow_all())
        .run();
    let failed = match refused {
        Err(_) => true,
        Ok(branch) => matches!(branch.info().status, BranchStatus::Failed { .. }),
    };
    assert!(failed);
    assert!(fake.actor_names().is_empty());
}
