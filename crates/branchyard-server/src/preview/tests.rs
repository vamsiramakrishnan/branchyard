//! Hermetic converter, cache and resource-boundary fixtures.

use super::cache::{
    cleanup_cache, fresh, helper_files, read_cached, read_regular, reusable_cache, CACHE_ENTRIES,
};
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
pub(crate) fn fake_service(scratch: &Scratch, body: &str) -> PreviewService {
    let python = scratch.join("python");
    script(&python, "#!/bin/sh\nprintf '%s\\n' '{\"executable\":\"/usr/bin/python3\",\"prefix\":\"/usr\",\"base_prefix\":\"/usr\",\"version\":\"fixture\"}'\n");
    let bwrap = scratch.join("bwrap");
    script(&bwrap, &format!("#!/bin/sh\nset -eu\noutput=''\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = --bind ]; then output=\"$2\"; shift 3; else shift; fi\ndone\n{body}\n"));
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
    assert!(fs::read_dir(&failure.root)
        .unwrap()
        .all(|entry| !entry.unwrap().path().is_dir()));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn images_are_addressed_only_through_the_matching_authorized_options() {
    let scratch = Scratch::new("preview-image");
    let body = "mkdir \"$output/derived\"\nprintf '\\211PNG\\r\\n\\032\\n' > \"$output/derived/page-1.png\"\nprintf '%s\\n' '{\"status\":\"passed\",\"kind\":\"pages\",\"format\":\"pdf\",\"page\":1,\"pages\":1,\"image\":\"page-1.png\",\"width\":1,\"height\":1,\"warnings\":[],\"path\":\"/private/converter/path\"}'";
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
    let first = service.admit().unwrap();
    let second = service.admit().unwrap();
    let error = service
        .preview(original(), PreviewOptions::default())
        .await
        .unwrap_err();
    assert_eq!(error.body.code, "preview_busy");
    let malformed = service.admit_asset("../file.png").err().unwrap();
    assert_eq!(malformed.body.code, "unknown_preview_asset");
    // A permit already owns its admission. The missing interpreter, rather
    // than a second capacity acquisition, must be the next observed boundary.
    let error = second
        .preview(original(), PreviewOptions::default())
        .await
        .unwrap_err();
    assert_eq!(error.body.code, "preview_unavailable");
    drop(first);
}

#[cfg(target_os = "linux")]
fn executing_service(scratch: &Scratch, python: PathBuf) -> PreviewService {
    // This fake only translates namespace paths and executes the real trusted
    // helper. It does not create directories or fake format responses.
    let bwrap = scratch.join("execute-helper");
    script(
        &bwrap,
        r#"#!/usr/bin/python3
import os
import subprocess
import sys
args = sys.argv[1:]
split = args.index('--')
bindings = []
index = 0
while index < split:
    if args[index] in ('--bind', '--ro-bind'):
        bindings.append((args[index + 2], args[index + 1]))
        index += 3
    else:
        index += 1
bindings.sort(key=lambda pair: len(pair[0]), reverse=True)
def translate(value):
    for destination, source in bindings:
        if value == destination or value.startswith(destination + '/'):
            return source + value[len(destination):]
    return value
command = [translate(value) for value in args[split + 1:]]
raise SystemExit(subprocess.run(command, env={'PATH': '/usr/bin:/bin', 'LANG': 'C.UTF-8'}).returncode)
"#,
    );
    PreviewService::with_settings(scratch.join("cache"), sandbox::Settings { python, bwrap })
        .unwrap()
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn real_embedded_csv_and_text_helpers_publish_without_output_directories() {
    let scratch = Scratch::new("preview-real-json");
    let service = executing_service(&scratch, "/usr/bin/python3".into());
    let csv = service
        .preview(original(), PreviewOptions::default())
        .await
        .unwrap();
    assert_eq!(csv["status"], "passed");
    assert_eq!(csv["rows"][0][0], "001");
    assert_eq!(csv["rows"][0][1], "=1+1");
    assert_eq!(
        csv,
        service
            .preview(original(), PreviewOptions::default())
            .await
            .unwrap()
    );
    let mut text = original();
    text.name = "notes.md".into();
    text.bytes = b"# Evidence\n<script>literal text</script>\n".to_vec();
    let response = service
        .preview(text, PreviewOptions::default())
        .await
        .unwrap();
    assert_eq!(response["kind"], "text");
    assert_eq!(
        response["text"],
        "# Evidence\n<script>literal text</script>\n"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn valid_wide_csv_is_paginated_before_stdout_reaches_its_byte_bound() {
    let scratch = Scratch::new("preview-wide-csv");
    let service = executing_service(&scratch, "/usr/bin/python3".into());
    let mut source = (0..30)
        .map(|index| format!("column_{index}"))
        .collect::<Vec<_>>()
        .join(",");
    source.push('\n');
    let row = vec!["x".repeat(1000); 30].join(",") + "\n";
    for _ in 0..100 {
        source.push_str(&row);
    }
    let mut first_input = original();
    first_input.bytes = source.clone().into_bytes();
    let first = service
        .preview(first_input, PreviewOptions::default())
        .await
        .unwrap();
    assert_eq!(first["status"], "passed");
    assert!(serde_json::to_vec(&first).unwrap().len() < MAX_RESPONSE_BYTES as usize);
    let rows = first["rows"].as_array().unwrap();
    assert!(!rows.is_empty() && rows.len() < 100);
    let offset = first["next_offset"].as_u64().unwrap() as u32;
    assert_eq!(offset as usize, rows.len());
    assert!(first["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| warning
            .as_str()
            .is_some_and(|warning| warning.contains("response bytes"))));
    let mut next_input = original();
    next_input.bytes = source.into_bytes();
    let next = service
        .preview(
            next_input,
            PreviewOptions {
                offset,
                ..PreviewOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(rows.len() + next["rows"].as_array().unwrap().len(), 100);
    assert!(next["next_offset"].is_null());
    assert_eq!(next["rows"][0][0], "x".repeat(1000));
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires the explicitly selected artifact-runtime Python with openpyxl"]
async fn real_multisheet_xlsx_helper_publishes_and_reuses_a_table_cache() {
    let scratch = Scratch::new("preview-real-xlsx");
    let python = std::env::var_os("BRANCHYARD_PREVIEW_TEST_PYTHON")
        .map_or_else(|| PathBuf::from("/usr/bin/python3"), PathBuf::from);
    let path = scratch.join("book.xlsx");
    let status = std::process::Command::new(&python).args(["-I", "-c", "import sys;from openpyxl import Workbook;w=Workbook();s=w.active;s.title='Inputs';s.append(['account','formula']);s.append(['001','=1+1']);t=w.create_sheet('Costs');t.append(['cost']);t.append([12]);w.save(sys.argv[1])"]).arg(&path).env_clear().env("PATH", "/usr/bin:/bin").status().unwrap();
    assert!(status.success());
    let bytes = fs::read(&path).unwrap();
    let service = executing_service(&scratch, python);
    let input = || PreviewInput {
        bytes: bytes.clone(),
        name: "book.xlsx".into(),
        media_type: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet".into(),
        digest: "fixture-xlsx".into(),
        tenant: "research".into(),
    };
    let response = service
        .preview(input(), PreviewOptions::default())
        .await
        .unwrap();
    assert_eq!(response["status"], "passed");
    assert_eq!(response["sheets"], json!(["Inputs", "Costs"]));
    assert_eq!(response["rows"][0], json!(["001", "=1+1"]));
    assert_eq!(
        response,
        service
            .preview(input(), PreviewOptions::default())
            .await
            .unwrap()
    );
    let costs = service
        .preview(
            input(),
            PreviewOptions {
                sheet: Some("Costs".into()),
                ..PreviewOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(costs["rows"][0][0], 12);
    assert_eq!(fs::read(path).unwrap(), bytes);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn two_services_share_cache_eviction_and_pin_images_before_unlocking() {
    let scratch = Scratch::new("preview-concurrent-cache");
    let body = "mkdir \"$output/derived\"\nprintf '\\211PNG\\r\\n\\032\\n' > \"$output/derived/page-1.png\"\nprintf '%s\\n' '{\"status\":\"passed\",\"kind\":\"pages\",\"format\":\"pdf\",\"page\":1,\"pages\":1,\"image\":\"page-1.png\",\"width\":1,\"height\":1,\"warnings\":[]}'";
    let first = fake_service(&scratch, body);
    let second = PreviewService::with_settings(first.root.clone(), first.settings.clone()).unwrap();
    for index in 0..CACHE_ENTRIES {
        let directory = first.root.join(format!("{index:064x}"));
        fs::create_dir(&directory).unwrap();
        fs::write(
            directory.join("result.json"),
            serde_json::to_vec(&table()).unwrap(),
        )
        .unwrap();
    }
    let a = async {
        for offset in 0..24 {
            assert_eq!(
                first
                    .asset(
                        original(),
                        PreviewOptions {
                            offset,
                            ..PreviewOptions::default()
                        },
                        "page-1.png"
                    )
                    .await
                    .unwrap(),
                b"\x89PNG\r\n\x1a\n"
            );
        }
    };
    let b = async {
        for offset in 1000..1024 {
            assert_eq!(
                second
                    .preview(
                        original(),
                        PreviewOptions {
                            offset,
                            ..PreviewOptions::default()
                        }
                    )
                    .await
                    .unwrap()["status"],
                "passed"
            );
        }
    };
    tokio::join!(a, b);
    let options = PreviewOptions {
        offset: 23,
        ..PreviewOptions::default()
    };
    let runtime = first.settings.runtime().await.unwrap();
    let key = cache_key(&original(), &options, &runtime.identity()).unwrap();
    let pinned = cache::lookup(
        first.root.clone(),
        key.clone(),
        first.admit().unwrap().materialization_guard(),
    )
    .await
    .unwrap()
    .unwrap();
    fs::remove_dir_all(first.root.join(key)).unwrap();
    assert_eq!(pinned.image.unwrap(), b"\x89PNG\r\n\x1a\n");
    assert_eq!(
        first
            .asset(original(), options, "page-1.png")
            .await
            .unwrap(),
        b"\x89PNG\r\n\x1a\n"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_cancelled_request_keeps_capacity_until_its_blocking_reader_finishes() {
    let scratch = Scratch::new("preview-materialization");
    let service = fake_service(&scratch, "exit 1");
    let permit = service.admit().unwrap();
    let retained = permit.materialization_guard();
    let (started, ready) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let job = tokio::task::spawn_blocking(move || {
        let _retained = retained;
        started.send(()).unwrap();
        wait.recv().unwrap();
    });
    ready.await.unwrap();
    drop(permit);
    let second = service.admit().unwrap();
    assert_eq!(service.admit().err().unwrap().body.code, "preview_busy");
    release.send(()).unwrap();
    job.await.unwrap();
    assert!(service.admit().is_ok());
    drop(second);
}
