//! Hermetic converter, cache and resource-boundary fixtures.

use super::cache::{fresh, helper_files, read_cached, reusable_cache, CACHE_ENTRIES};
use super::*;
use branchyard_testkit::Scratch;
use std::path::Path as FilePath;

fn original() -> PreviewInput {
    PreviewInput {
        bytes: b"account,value\n001,=1+1\n".to_vec(),
        name: "accounts.csv".into(),
        media_type: "text/csv".into(),
        digest: "immutable-source-digest".into(),
        tenant: "research".into(),
    }
}

fn table() -> Value {
    json!({"status":"passed","kind":"table","format":"csv", "columns":["account","value"],"rows":[["001","=1+1"]],"offset":0,"next_offset":null,"warnings":[]})
}

#[test]
fn query_validation_rejects_ambiguity_and_unbounded_work() {
    let options =
        PreviewOptions::parse(Some("view=table&sheet=Quarter%201&offset=2&limit=4")).unwrap();
    assert_eq!(options.sheet.as_deref(), Some("Quarter 1"));
    assert_eq!(options.limit, 4);
    assert_eq!(options.offset, 2);
    assert_eq!(options.view.as_str(), "table");
    for invalid in [
        "page=0",
        "page=501",
        "limit=0",
        "limit=501",
        "offset=100001",
        "page=-1",
        "view=html",
        "sheet=",
        "page=1&page=2",
        "unknown=1",
        "page",
    ] {
        assert!(PreviewOptions::parse(Some(invalid)).is_err(), "{invalid}");
    }
    assert_eq!(
        PreviewOptions::parse(None).unwrap(),
        PreviewOptions::default()
    );
}

#[test]
fn cache_identity_binds_tenant_bytes_options_runtime_and_format() {
    let first = cache_key(&original(), &PreviewOptions::default(), "runtime1").unwrap();
    let mut changed = original();
    changed.tenant = "other-tenant".into();
    assert_ne!(
        first,
        cache_key(&changed, &PreviewOptions::default(), "runtime1").unwrap()
    );
    changed = original();
    changed.bytes.push(b'x');
    assert_ne!(
        first,
        cache_key(&changed, &PreviewOptions::default(), "runtime1").unwrap()
    );
    changed = original();
    changed.name = "accounts.xlsx".into();
    assert_ne!(
        first,
        cache_key(&changed, &PreviewOptions::default(), "runtime1").unwrap()
    );
    assert_ne!(
        first,
        cache_key(
            &original(),
            &PreviewOptions {
                page: 2,
                ..PreviewOptions::default()
            },
            "runtime1"
        )
        .unwrap()
    );
    assert_ne!(
        first,
        cache_key(&original(), &PreviewOptions::default(), "runtime2").unwrap()
    );
    assert_eq!(suffix("../../a.XLSX").unwrap(), "xlsx");
    assert!(suffix("document.xlsm").is_err());
    assert!(suffix("document.html").is_err());
}

#[test]
fn embedded_helper_materialization_stays_inside_the_trusted_package() {
    let scratch = Scratch::new("preview-embed");
    materialize_helper(scratch.path()).unwrap();
    assert!(scratch.join("launcher.py").is_file());
    assert!(scratch.join("branchyard_artifacts/__main__.py").is_file());
    assert!(!scratch.join("SKILL.md").exists());
    assert!(!scratch.join("INDEX.md").exists());
    assert!(helper_files()
        .unwrap()
        .all(|(p, _)| p.starts_with("branchyard_artifacts/")
            && !p.split('/').any(|part| part == "..")));
}

#[test]
fn cache_reads_require_regular_bounded_files_and_valid_responses() {
    let scratch = Scratch::new("preview-cache");
    assert!(read_cached(&scratch.join("missing")).unwrap().is_none());
    let directory = scratch.join("cache");
    fs::create_dir(&directory).unwrap();
    fs::write(
        directory.join("result.json"),
        serde_json::to_vec(&table()).unwrap(),
    )
    .unwrap();
    assert_eq!(read_cached(&directory).unwrap().unwrap(), table());
    let bad = json!({"status":"passed","kind":"table","columns":[],"rows":[{}]});
    assert!(validate_response(&bad, &directory).is_err());
    let too_many = json!({"status":"passed","kind":"table","columns":[],"rows":vec![Vec::<String>::new();501]});
    assert!(validate_response(&too_many, &directory).is_err());
    fs::write(directory.join("oversized"), vec![0_u8; 40]).unwrap();
    assert!(read_regular(&directory.join("oversized"), 16).is_err());
    fs::write(directory.join("result.json"), b"partial JSON").unwrap();
    assert!(reusable_cache(&directory).unwrap().is_none());
    assert!(!directory.exists());
    assert!(safe_asset("slide-1.png"));
    for invalid in [
        "../slide.png",
        "/source.png",
        ".hidden.png",
        "file.svg",
        "a/b.png",
        "x..png",
    ] {
        assert!(!safe_asset(invalid), "{invalid}");
    }
}

#[cfg(unix)]
#[test]
fn converter_cannot_publish_symlink_directories_or_files() {
    use std::os::unix::fs::symlink;
    let scratch = Scratch::new("preview-links");
    let outside = scratch.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("original"), "unchanged").unwrap();
    let link = scratch.join("derived");
    symlink(&outside, &link).unwrap();
    assert!(validate_response(&table(), &link).is_err());
    assert!(!outside.join("result.json").exists());
    let valid = scratch.join("valid");
    fs::create_dir(&valid).unwrap();
    symlink(outside.join("original"), valid.join("extra")).unwrap();
    assert!(validate_response(&table(), &valid).is_err());
    assert!(read_regular(&valid.join("extra"), 100).is_err());
    assert_eq!(
        fs::read_to_string(outside.join("original")).unwrap(),
        "unchanged"
    );
}

#[test]
fn eviction_only_removes_owned_cache_entries_and_keeps_other_files() {
    let scratch = Scratch::new("preview-eviction");
    fs::write(scratch.join("operator-note"), "keep").unwrap();
    for index in 0..CACHE_ENTRIES {
        let directory = scratch.join(format!("{index:064x}"));
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("result.json"), b"{}").unwrap();
    }
    cleanup_cache(scratch.path()).unwrap();
    let remaining = fs::read_dir(scratch.path()).unwrap().count();
    assert_eq!(remaining, CACHE_ENTRIES);
    assert_eq!(
        fs::read_to_string(scratch.join("operator-note")).unwrap(),
        "keep"
    );
    assert!(fresh(5, 6));
    assert!(fresh(6, 5));
    assert!(!fresh(0, 24 * 60 * 60 * 1000));
}

#[cfg(target_os = "linux")]
fn script(path: &FilePath, contents: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[cfg(target_os = "linux")]
fn fake_service(scratch: &Scratch, body: &str) -> PreviewService {
    let python = scratch.join("python");
    script(&python, "#!/bin/sh\nprintf '%s\\n' '{\"executable\":\"/usr/bin/python3\",\"prefix\":\"/usr\",\"base_prefix\":\"/usr\",\"version\":\"fixture\"}'\n");
    let bwrap = scratch.join("bwrap");
    script(&bwrap, &format!("#!/bin/sh\nset -eu\noutput=''\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = --bind ]; then output=\"$2\"; shift 3; else shift; fi\ndone\nmkdir -p \"$output/derived\"\n{body}\n"));
    PreviewService::with_settings(scratch.join("cache"), sandbox::Settings { python, bwrap })
        .unwrap()
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn derivation_is_cached_and_failed_conversion_is_never_published() {
    let scratch = Scratch::new("preview-service");
    let calls = scratch.join("calls");
    let body = format!(
        "printf x >> '{}'\nprintf '%s\\n' '{}'",
        calls.display(),
        table()
    );
    let service = fake_service(&scratch, &body);
    let first = service
        .preview(original(), PreviewOptions::default())
        .await
        .unwrap();
    let second = service
        .preview(original(), PreviewOptions::default())
        .await
        .unwrap();
    assert_eq!(first, second);
    assert_eq!(first["rows"][0][0], "001");
    assert_eq!(first["rows"][0][1], "=1+1");
    assert_eq!(fs::read(&calls).unwrap(), b"x");
    let mut tenant = original();
    tenant.tenant = "second".into();
    service
        .preview(tenant, PreviewOptions::default())
        .await
        .unwrap();
    assert_eq!(fs::read(&calls).unwrap(), b"xx");
    assert!(fs::read_dir(service.root.clone())
        .unwrap()
        .all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".stage-")));

    let broken = Scratch::new("preview-failed");
    let failure = fake_service(
        &broken,
        "printf '%s\\n' '{\"status\":\"unavailable\",\"next\":\"install runtime\"}'\nexit 2",
    );
    let response = failure
        .preview(original(), PreviewOptions::default())
        .await
        .unwrap();
    assert_eq!(response["status"], "unavailable");
    assert_eq!(fs::read_dir(&failure.root).unwrap().count(), 0);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn images_are_addressed_only_through_the_matching_authorized_options() {
    let scratch = Scratch::new("preview-image");
    let body = "printf '\\211PNG\\r\\n\\032\\n' > \"$output/derived/page-1.png\"\nprintf '%s\\n' '{\"status\":\"passed\",\"kind\":\"pages\",\"format\":\"pdf\",\"page\":1,\"pages\":1,\"image\":\"page-1.png\",\"width\":1,\"height\":1,\"warnings\":[],\"path\":\"/private/converter/path\"}'";
    let service = fake_service(&scratch, body);
    let result = service
        .preview(original(), PreviewOptions::default())
        .await
        .unwrap();
    assert!(result.get("path").is_none());
    let bytes = service
        .asset(original(), PreviewOptions::default(), "page-1.png")
        .await
        .unwrap();
    assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    let error = service
        .asset(original(), PreviewOptions::default(), "page-2.png")
        .await
        .unwrap_err();
    assert_eq!(error.body.code, "unknown_preview_asset");
    assert!(service
        .asset(original(), PreviewOptions::default(), "../source.png")
        .await
        .is_err());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn missing_executables_are_actionable_and_conversion_slots_are_bounded() {
    let scratch = Scratch::new("preview-unavailable");
    let service = PreviewService::with_settings(
        scratch.join("cache"),
        sandbox::Settings {
            python: scratch.join("missing-python"),
            bwrap: scratch.join("missing-bwrap"),
        },
    )
    .unwrap();
    let error = service
        .preview(original(), PreviewOptions::default())
        .await
        .unwrap_err();
    assert_eq!(error.body.code, "preview_unavailable");
    assert!(error.body.message.contains("BRANCHYARD_PREVIEW_PYTHON"));
    let permits = service.slots.acquire_many(2).await.unwrap();
    let error = service
        .preview(original(), PreviewOptions::default())
        .await
        .unwrap_err();
    assert_eq!(error.body.code, "preview_busy");
    drop(permits);
}
