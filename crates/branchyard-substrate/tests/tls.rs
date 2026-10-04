//! TLS on both hops: the `Control` API over `https://` with a private
//! authority and a client certificate, and the router and bridges over
//! `https://`, all served by the fake with certificates generated here.
//! A client that trusts another authority, or has no client certificate,
//! is refused; plain HTTP is refused off loopback unless explicitly
//! allowed. Not evidence about a Substrate cluster.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use branchyard_bridge::Signer;
use branchyard_sandbox::{ExecSpec, ProviderError, SandboxProvider, SandboxSpec};
use branchyard_substrate::fake::{FakeCluster, FakeTls};
use branchyard_substrate::template::{bridge_template, BridgeTemplate};
use branchyard_substrate::{pb, transfer, Config, SubstrateProvider};
use common::{bridge_binary, Scratch, ATESPACE, TEMPLATE};

/// A self-signed authority, its PEM written to `path`.
fn authority(path: &Path) -> rcgen::Issuer<'static, rcgen::KeyPair> {
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(Vec::new()).unwrap();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "branchyard test CA");
    let cert = params.self_signed(&key).unwrap();
    fs::write(path, cert.pem()).unwrap();
    rcgen::Issuer::new(params, key)
}

/// A certificate from `ca` for loopback, written as `<stem>.crt` and
/// `<stem>.key`.
fn leaf(ca: &rcgen::Issuer<'_, rcgen::KeyPair>, dir: &Path, stem: &str) -> (PathBuf, PathBuf) {
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
    (cert_path, key_path)
}

struct Pki {
    ca: PathBuf,
    client_cert: PathBuf,
    client_key: PathBuf,
    other_ca: PathBuf,
    stranger_cert: PathBuf,
    stranger_key: PathBuf,
}

/// A TLS fake cluster requiring client certificates from its own
/// authority, and the files a client needs.
fn cluster(scratch: &Scratch) -> (FakeCluster, Pki, PathBuf) {
    let pki = scratch.path("pki");
    fs::create_dir_all(&pki).unwrap();
    let ca = authority(&pki.join("ca.pem"));
    let (cert, key) = leaf(&ca, &pki, "server");
    let (client_cert, client_key) = leaf(&ca, &pki, "client");
    let other = authority(&pki.join("other-ca.pem"));
    let (stranger_cert, stranger_key) = leaf(&other, &pki, "stranger");
    let fake = FakeCluster::start_tls(
        ATESPACE,
        Some(bridge_binary().to_path_buf()),
        &scratch.path("cluster"),
        FakeTls {
            ca: pki.join("ca.pem"),
            cert,
            key,
            client_ca: Some(pki.join("ca.pem")),
        },
    );
    let key = scratch.path("bridge.key");
    let signer = Signer::write(&key).unwrap();
    fake.add_template(bridge_template(
        ATESPACE,
        &BridgeTemplate {
            name: TEMPLATE.into(),
            image: "registry.example/branchyard@sha256:00".into(),
            bridge: "/usr/local/bin/branchyard-bridge".into(),
            public_key: signer.public_key(),
            sandbox_class: pb::SandboxClass::Gvisor,
            sandbox_config: "gvisor".into(),
            storage_location: "gs://bucket/branchyard".into(),
        },
    ));
    let files = Pki {
        ca: pki.join("ca.pem"),
        client_cert,
        client_key,
        other_ca: pki.join("other-ca.pem"),
        stranger_cert,
        stranger_key,
    };
    (fake, files, key)
}

fn config(fake: &FakeCluster, pki: &Pki, key: &Path) -> Config {
    let mut config = Config::new(fake.endpoint(), ATESPACE, TEMPLATE, fake.router())
        .signer(Signer::read(key).unwrap());
    config.ready_timeout = Duration::from_secs(20);
    config.ca = Some(pki.ca.clone());
    config.client_cert = Some(pki.client_cert.clone());
    config.client_key = Some(pki.client_key.clone());
    config
}

fn run(provider: &SubstrateProvider, name: &str, script: &str) -> String {
    let spec = ExecSpec {
        argv: vec!["sh".into(), "-c".into(), script.into()],
        cwd: "/".into(),
        env: [("PATH".into(), std::env::var_os("PATH").unwrap_or_default())].into(),
    };
    let mut process = provider.exec(name, &spec).unwrap();
    drop(process.take_stdin());
    let mut out = String::new();
    process
        .take_stdout()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    assert!(process.wait().unwrap().success(), "{script}");
    out
}

#[test]
fn both_hops_run_over_tls_with_a_private_authority_and_a_client_certificate() {
    let scratch = Scratch::new("tls");
    let (fake, pki, key) = cluster(&scratch);
    assert!(fake.endpoint().starts_with("https://"));
    assert!(fake.router().starts_with("https://"));
    let provider = SubstrateProvider::connect(config(&fake, &pki, &key)).unwrap();
    assert!(provider.capabilities().exec);
    provider.ensure(&SandboxSpec::new("secure")).unwrap();
    assert_eq!(run(&provider, "secure", "echo over tls"), "over tls\n");

    // Code crosses the TLS router too.
    let endpoint = provider.endpoint("secure").unwrap();
    assert!(endpoint.url().starts_with("https://"));
    let home = scratch.path("home");
    fs::create_dir_all(&home).unwrap();
    fs::write(home.join("settings"), "v1\n").unwrap();
    let guest = scratch.path("guest-home");
    transfer::push_tree(&endpoint, &home, &guest).unwrap();
    fs::write(guest.join("settings"), "v2\n").unwrap();
    transfer::pull_tree(&endpoint, &guest, &home).unwrap();
    assert_eq!(fs::read_to_string(home.join("settings")).unwrap(), "v2\n");

    provider.stop("secure").unwrap();
    provider.destroy("secure").unwrap();
    assert!(fake.actor_names().is_empty());
}

#[test]
fn a_wrong_authority_or_a_missing_client_certificate_is_refused() {
    let scratch = Scratch::new("tls-refused");
    let (fake, pki, key) = cluster(&scratch);

    // The Control API's certificate from an authority the client does not
    // trust.
    let mut wrong_ca = config(&fake, &pki, &key);
    wrong_ca.ca = Some(pki.other_ca.clone());
    wrong_ca.router_ca = Some(pki.ca.clone());
    let provider = SubstrateProvider::connect(wrong_ca).unwrap();
    assert!(provider.template_capabilities().is_err());
    assert!(provider.ensure(&SandboxSpec::new("never")).is_err());
    // The public roots do not trust it either.
    let mut public = config(&fake, &pki, &key);
    public.ca = None;
    public.router_ca = Some(pki.ca.clone());
    let provider = SubstrateProvider::connect(public).unwrap();
    assert!(provider.template_capabilities().is_err());

    // No client certificate, or one from another authority.
    let mut anonymous = config(&fake, &pki, &key);
    anonymous.client_cert = None;
    anonymous.client_key = None;
    let provider = SubstrateProvider::connect(anonymous).unwrap();
    assert!(provider.template_capabilities().is_err());
    let mut stranger = config(&fake, &pki, &key);
    stranger.client_cert = Some(pki.stranger_cert.clone());
    stranger.client_key = Some(pki.stranger_key.clone());
    let provider = SubstrateProvider::connect(stranger).unwrap();
    assert!(provider.template_capabilities().is_err());
    assert!(fake.actor_names().is_empty(), "nothing was created");

    // The router's certificate from an authority the client does not
    // trust: refused at once, not after the ready timeout.
    let mut wrong_router = config(&fake, &pki, &key);
    wrong_router.router_ca = Some(pki.other_ca.clone());
    let provider = SubstrateProvider::connect(wrong_router).unwrap();
    assert!(provider.template_capabilities().is_ok());
    let started = Instant::now();
    match provider.ensure(&SandboxSpec::new("unverified")) {
        Err(ProviderError::Runtime(why)) => assert!(why.contains("TLS"), "{why}"),
        other => panic!("expected a TLS failure, got {other:?}"),
    }
    assert!(started.elapsed() < Duration::from_secs(15));
    provider.destroy("unverified").unwrap();
    assert!(fake.actor_names().is_empty());
}

#[test]
fn plain_http_is_refused_off_loopback_and_tls_files_need_tls() {
    let scratch = Scratch::new("tls-config");
    let ca = scratch.path("ca.pem");
    let issuer = authority(&ca);
    let (cert, key) = leaf(&issuer, &scratch.0, "client");
    let router = "https://router.example/{atespace}/{actor}/";
    let base = |endpoint: &str, router: &str| Config::new(endpoint, ATESPACE, TEMPLATE, router);
    let refused = |config: Config| match SubstrateProvider::connect(config.clone()) {
        Err(ProviderError::Invalid(why)) => why,
        other => panic!("{config:?} was accepted: {other:?}"),
    };

    // Loopback in the clear is fine; another host is not, unless allowed.
    for fine in [
        base("http://127.0.0.1:1", "http://localhost:2/{actor}/"),
        base("http://[::1]:1", "ws://127.0.0.2:2/{actor}/"),
        base("https://control.example", router),
        base("https://control.example", "wss://router.example/{actor}/"),
    ] {
        fine.check().unwrap();
    }
    let why = refused(base("http://control.example:8080", router));
    assert!(why.contains("unencrypted"), "{why}");
    let why = refused(base(
        "https://control.example",
        "http://router.example/{actor}/",
    ));
    assert!(why.contains("--substrate-insecure"), "{why}");
    let mut allowed = base(
        "http://control.example:8080",
        "ws://router.example/{actor}/",
    );
    allowed.insecure = true;
    allowed.check().unwrap();

    // TLS files for a URL in the clear, half a client identity, and an
    // unreadable or malformed file are all refused before anything runs.
    let mut ca_for_plain = base("http://127.0.0.1:1", router);
    ca_for_plain.ca = Some(ca.clone());
    refused(ca_for_plain);
    let mut router_ca_for_plain = base("https://control.example", "http://127.0.0.1:1/{actor}/");
    router_ca_for_plain.router_ca = Some(ca.clone());
    refused(router_ca_for_plain);
    let mut half = base("https://control.example", router);
    half.client_cert = Some(cert.clone());
    refused(half);
    let mut missing = base("https://control.example", router);
    missing.ca = Some(scratch.path("missing.pem"));
    refused(missing);
    let mut not_a_ca = base("https://control.example", router);
    not_a_ca.ca = Some(key.clone());
    refused(not_a_ca);
    let mut mismatched = base("https://control.example", router);
    mismatched.client_cert = Some(cert);
    mismatched.client_key = Some(ca);
    refused(mismatched);
    refused(base("ftp://control.example", router));
}
