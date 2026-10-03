//! Every backend passes one conformance suite: the directory backend and
//! memory always; S3, Cloud Storage and Azure Blob through in-process
//! stand-ins that implement their conditional semantics and check their
//! signatures; a git remote (a local bare repository); and MinIO,
//! fake-gcs-server or Azurite when their endpoints are configured. Plus
//! resumable uploads stopped part way and continued, and every token
//! source and KMS client against its stand-in.

use std::sync::Arc;

use branchyard_sync::auth::google::{GoogleAuth, ServiceAccount};
use branchyard_sync::kms::{AwsKms, AzureKeyVault, GcpKms, Wrapper};
use branchyard_sync::store::conformance::{check, Options};
use branchyard_sync::store::file::FileStore;
use branchyard_sync::store::{MemoryJournal, ObjectStore, UploadJournal};
use branchyard_sync::testing::azure::MockAzure;
use branchyard_sync::testing::cloud::{self, MockKms, MockMetadata};
use branchyard_sync::testing::gcs::{self, MockGcs};
use branchyard_sync::testing::s3::MockS3;

const PART: usize = 4096;
const LARGE: usize = PART * 3 + 123;

fn options() -> Options {
    Options {
        large: LARGE,
        sizes: true,
    }
}

#[test]
fn the_directory_backend_conforms() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::open(dir.path()).unwrap().with_part_size(PART);
    check(Arc::new(store), options());
    // Nothing is left half-written.
    let tmp: Vec<_> = std::fs::read_dir(dir.path().join(".tmp"))
        .unwrap()
        .collect();
    assert!(tmp.is_empty(), "{tmp:?}");
}

#[test]
fn the_memory_backend_conforms() {
    check(
        Arc::new(branchyard_sync::store::memory::MemoryStore::new()),
        options(),
    );
}

#[test]
fn s3_conforms_against_the_stand_in() {
    let s3 = MockS3::start("bucket", 3);
    check(Arc::new(s3.store("bucket", "pre/fix", PART)), options());
    // The conditions went on the wire, and listing paged.
    let log = s3.server.requests();
    assert!(log
        .iter()
        .any(|l| l.contains("list-type=2") && l.contains("continuation-token")));
    assert!(log
        .iter()
        .any(|l| l.starts_with("POST") && l.contains("uploads")));
    assert_eq!(s3.uploads_open(), 0);
}

#[test]
fn gcs_conforms_against_the_stand_in() {
    let g = MockGcs::start("bucket", Some(gcs::TOKEN), 3);
    check(Arc::new(g.store("bucket", "pre", PART)), options());
    let log = g.server.requests();
    assert!(log.iter().any(|l| l.contains("ifGenerationMatch=0")));
    assert!(log.iter().any(|l| l.contains("uploadType=resumable")));
    assert!(log.iter().any(|l| l.contains("pageToken")));
}

#[test]
fn azure_conforms_against_the_stand_in() {
    let az = MockAzure::start("sync", 3);
    check(Arc::new(az.store("sync", "pre", PART)), options());
    let log = az.server.requests();
    assert!(log.iter().any(|l| l.contains("comp=blocklist")));
    assert!(log.iter().any(|l| l.contains("marker=")));
    assert_eq!(az.staged(), 0);
}

#[test]
fn a_git_remote_conforms() {
    let dir = tempfile::tempdir().unwrap();
    let remote = dir.path().join("remote.git");
    branchyard_sync::testing::git(dir.path(), &["init", "--quiet", "--bare", "remote.git"]);
    let store = branchyard_sync::store::git::GitStore::open(
        &format!("file://{}", remote.display()),
        Some(&dir.path().join("cache")),
    )
    .unwrap();
    check(
        Arc::new(store),
        Options {
            large: LARGE,
            sizes: false,
        },
    );
}

/// The real emulators, when configured: `BRANCHYARD_SYNC_TEST_S3`
/// (an `s3://bucket?endpoint=...` URL, with `AWS_ACCESS_KEY_ID` and
/// `AWS_SECRET_ACCESS_KEY`; MinIO), `BRANCHYARD_SYNC_TEST_GCS`
/// (`gs://bucket?endpoint=...`; fake-gcs-server) and
/// `BRANCHYARD_SYNC_TEST_AZURE` (`az://devstoreaccount1/container?endpoint=...`
/// with `AZURE_STORAGE_KEY`; Azurite). Skipped, saying why, otherwise.
#[test]
fn configured_emulators_conform() {
    for (var, what) in [
        ("BRANCHYARD_SYNC_TEST_S3", "MinIO"),
        ("BRANCHYARD_SYNC_TEST_GCS", "fake-gcs-server"),
        ("BRANCHYARD_SYNC_TEST_AZURE", "Azurite"),
    ] {
        match std::env::var(var).ok().filter(|v| !v.is_empty()) {
            Some(url) => {
                let store = branchyard_sync::store::open(&url).unwrap();
                check(
                    store,
                    Options {
                        large: 64 << 10,
                        sizes: true,
                    },
                );
                println!("{what}: conformance passed against {url}");
            }
            None => println!("skipped {what}: {var} is not set"),
        }
    }
}

/// An upload stopped after its first parts continues from them.
#[test]
fn resumable_uploads_continue_where_they_stopped() {
    let data: Vec<u8> = (0..LARGE).map(|i| (i % 253) as u8).collect();

    let s3 = MockS3::start("b", 100);
    let store = s3.store("b", "", PART);
    let journal = MemoryJournal::default();
    s3.faults.fail("PUT", "partNumber=3", 400, 0, 1);
    assert!(store.resumable_put("big", &data, &journal).is_err());
    assert!(!journal.entries().is_empty(), "the progress was journaled");
    store.resumable_put("big", &data, &journal).unwrap();
    assert_eq!(store.get("big").unwrap().data, data);
    let parts1 = s3
        .server
        .requests()
        .iter()
        .filter(|l| l.contains("partNumber=1&"))
        .count();
    assert_eq!(parts1, 1, "part 1 went up once");

    let g = MockGcs::start("b", Some(gcs::TOKEN), 100);
    let store = g.store("b", "", PART);
    let journal = MemoryJournal::default();
    g.faults.fail("PUT", "upload_id", 400, 1, 1);
    assert!(store.resumable_put("big", &data, &journal).is_err());
    assert_eq!(
        g.session_bytes(),
        vec![PART],
        "the session holds the first chunk"
    );
    store.resumable_put("big", &data, &journal).unwrap();
    assert_eq!(store.get("big").unwrap().data, data);
    let puts = g
        .server
        .requests()
        .iter()
        .filter(|l| l.starts_with("PUT"))
        .count();
    // 1 + 1 failed + 1 status query + 3 remaining.
    assert_eq!(puts, 6, "{:?}", g.server.requests());

    let az = MockAzure::start("c", 100);
    let store = az.store("c", "", PART);
    let journal = MemoryJournal::default();
    az.faults.fail("PUT", "comp=block&", 400, 2, 1);
    assert!(store.resumable_put("big", &data, &journal).is_err());
    assert_eq!(az.staged(), 2);
    store.resumable_put("big", &data, &journal).unwrap();
    assert_eq!(store.get("big").unwrap().data, data);
    let blocks = az
        .server
        .requests()
        .iter()
        .filter(|l| l.contains("comp=block&"))
        .count();
    assert_eq!(blocks, 5, "two staged, one failed, two after");

    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::open(dir.path()).unwrap().with_part_size(PART);
    let journal = MemoryJournal::default();
    journal.save("big", &format!("{}:{}", "0".repeat(32), PART));
    store.resumable_put("big", &data, &journal).unwrap();
    assert_eq!(
        store.get("big").unwrap().data,
        data,
        "a stale journal entry is ignored"
    );
}

#[test]
fn signatures_and_tokens_are_required() {
    let s3 = MockS3::start("b", 10);
    let mut wrong = MockS3::credentials();
    wrong.secret_key = "not the secret".into();
    let store = s3.store("b", "", PART).with_credentials(wrong);
    let e = store.put_if_absent("k", b"x").unwrap_err();
    assert_eq!(e.kind, branchyard_sync::Kind::Refused, "{e}");

    let g = MockGcs::start("b", Some(gcs::TOKEN), 10);
    let store = g.store("b", "", PART).with_auth(GoogleAuth::fixed("wrong"));
    assert_eq!(
        store.put_if_absent("k", b"x").unwrap_err().kind,
        branchyard_sync::Kind::Refused
    );

    let az = MockAzure::start("c", 10);
    let store = az.store("c", "", PART).with_credential(
        branchyard_sync::auth::azure::Credential::SharedKey {
            key: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
        },
    );
    assert_eq!(
        store.put_if_absent("k", b"x").unwrap_err().kind,
        branchyard_sync::Kind::Refused
    );
}

const TEST_KEY: &str = include_str!("../src/auth/testdata/rsa-test-key.pem");

#[test]
fn token_sources_work_against_their_stand_ins() {
    let meta = MockMetadata::start(TEST_KEY);
    let host = meta.server.url.trim_start_matches("http://").to_owned();
    // A service account's JWT, exchanged at its token URI.
    let auth = GoogleAuth::from_account(ServiceAccount {
        client_email: "sync@example.iam.gserviceaccount.com".into(),
        private_key: TEST_KEY.into(),
        token_uri: format!("{}/token", meta.server.url),
    });
    assert_eq!(
        auth.header(1_000).unwrap().unwrap(),
        format!("Bearer {}", cloud::OAUTH_TOKEN)
    );
    // Cached: no second exchange.
    auth.header(2_000).unwrap();
    assert_eq!(
        meta.server
            .requests()
            .iter()
            .filter(|l| l.contains("/token"))
            .count(),
        1
    );
    // The metadata server.
    let auth = GoogleAuth::from_metadata(&host);
    assert_eq!(
        auth.header(1_000).unwrap().unwrap(),
        format!("Bearer {}", cloud::METADATA_TOKEN)
    );
    // A GCS store authenticating with the service account's token.
    let g = MockGcs::start("b", Some(cloud::OAUTH_TOKEN), 10);
    let store = g
        .store("b", "", PART)
        .with_auth(GoogleAuth::from_account(ServiceAccount {
            client_email: "sync@example.iam.gserviceaccount.com".into(),
            private_key: TEST_KEY.into(),
            token_uri: format!("{}/token", meta.server.url),
        }));
    store.put_if_absent("k", b"via a service account").unwrap();
    // AWS IMDSv2, and an S3 store signing with what it returned.
    let creds = branchyard_sync::auth::aws::imds(&meta.server.url).unwrap();
    assert_eq!(creds.session_token.as_deref(), Some("imds-session"));
    assert!(creds.expires_ms.is_some());
    let s3 = MockS3::start("b", 10);
    s3.store("b", "", PART)
        .with_credentials(creds)
        .put_if_absent("k", b"via instance credentials")
        .unwrap();
    // An Azure managed identity.
    let identity = branchyard_sync::auth::azure::ManagedIdentity::new("https://storage.azure.com/")
        .with_endpoint(
            &format!("{}/identity", meta.server.url),
            cloud::IDENTITY_HEADER,
        );
    assert_eq!(
        identity.token(1_000).unwrap(),
        format!("{}:https://storage.azure.com/", cloud::IDENTITY_TOKEN)
    );
}

#[test]
fn kms_clients_wrap_and_unwrap_through_their_stand_in() {
    let kms = MockKms::start();
    let gcp = GcpKms::new(
        "projects/p/locations/global/keyRings/r/cryptoKeys/k",
        Some(&kms.server.url),
        GoogleAuth::fixed(cloud::KMS_TOKEN),
    )
    .unwrap();
    let aws = AwsKms::new("alias/sync", "us-east-1", Some(&kms.server.url))
        .unwrap()
        .with_credentials(MockS3::credentials());
    let azure = AzureKeyVault::new("vault.example", "sync", None, Some(&kms.server.url))
        .unwrap()
        .with_token(cloud::KMS_TOKEN);
    for wrapper in [&gcp as &dyn Wrapper, &aws, &azure] {
        let wrapped = wrapper.wrap(b"a tenant key secret").unwrap();
        assert!(!wrapped.contains("tenant"));
        assert_eq!(wrapper.unwrap(&wrapped).unwrap(), b"a tenant key secret");
        assert!(
            wrapper.unwrap("dGFtcGVyZWQ=").is_err(),
            "{}",
            wrapper.describe()
        );
    }
    let calls = kms.calls();
    for want in [
        "gcp:encrypt",
        "gcp:decrypt",
        "aws:Encrypt",
        "aws:Decrypt",
        "azure:wrapkey",
        "azure:unwrapkey",
    ] {
        assert!(calls.contains(&want.to_owned()), "{want} in {calls:?}");
    }
}
