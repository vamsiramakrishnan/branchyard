//! Hermetic Docker process, packet and cancellation regressions.

use super::*;
use branchyard_testkit::{wait, Scratch};
use std::fs;

fn runtime(executable: PathBuf) -> Runtime {
    Runtime {
        settings: Settings {
            executable,
            image: format!("sha256:{}", "a".repeat(64)),
            python: "/usr/local/bin/python3".into(),
        },
        image: format!("sha256:{}", "a".repeat(64)),
    }
}

#[test]
fn container_has_no_host_mounts_credentials_or_unbounded_resources() {
    let selected = runtime("docker".into());
    let args: Vec<_> = create_arguments(&selected)
        .into_iter()
        .map(|v| v.to_string_lossy().into_owned())
        .collect();
    for pair in [
        ["--network", "none"],
        ["--cap-drop", "ALL"],
        ["--security-opt", "no-new-privileges"],
        ["--user", "65532:65532"],
        ["--memory", "768m"],
        ["--memory-swap", "768m"],
        ["--pids-limit", "128"],
        ["--cpus", "1"],
        ["--pull", "never"],
        ["--log-driver", "none"],
        ["--signal=KILL", "40"],
    ] {
        assert!(args.windows(2).any(|v| v == pair), "{pair:?}");
    }
    assert!(args.contains(&"--read-only".into()));
    assert!(!args.iter().any(|v| matches!(
        v.as_str(),
        "--mount" | "--volume" | "-v" | "--env-file" | "--privileged" | "--pid" | "--device"
    )));
    assert!(args
        .iter()
        .any(|v| v == "/tmp:rw,noexec,nosuid,nodev,size=256m,mode=1777"));
    assert!(args
        .iter()
        .any(|v| v == "/output:rw,noexec,nosuid,nodev,size=64m,mode=1777"));
    assert!(args.iter().any(|v| v == &selected.image));
    let mut different = runtime("docker".into());
    different.image = format!("sha256:{}", "b".repeat(64));
    assert_ne!(selected.identity(), different.identity());
}

#[test]
fn image_identity_refuses_tags_and_transport_urls() {
    assert!(image_id(&format!("sha256:{}", "a".repeat(64))));
    for image in [
        "image:latest",
        "https://registry/image",
        "sha256:abcd",
        "sha256:../image",
    ] {
        assert!(!image_id(image));
    }
}

#[test]
fn packet_refuses_traversal_invalid_encoding_and_excessive_metadata() {
    let scratch = Scratch::new("container-preview-packet");
    let png = STANDARD.encode(b"\x89PNG\r\n\x1a\nfixture");
    let metadata = json!({"status":"passed","kind":"pages","image":"slide-1.png","pages":1});
    let packet = json!({"preview":metadata,"image":png});
    assert_eq!(
        unpack(&serde_json::to_vec(&packet).unwrap(), scratch.path()).unwrap(),
        metadata
    );
    assert_eq!(
        fs::read(scratch.join("derived/slide-1.png")).unwrap(),
        b"\x89PNG\r\n\x1a\nfixture"
    );
    for name in ["../escaped.png", "/escaped.png", "x/y.png"] {
        let mut bad = packet.clone();
        bad["preview"]["image"] = name.into();
        assert!(unpack(&serde_json::to_vec(&bad).unwrap(), scratch.path()).is_err());
    }
    let bad = json!({"preview":metadata,"image":"invalid!"});
    assert!(unpack(&serde_json::to_vec(&bad).unwrap(), scratch.path()).is_err());
    let bad = json!({"preview":{"status":"passed","text":"x".repeat(MAX_RESPONSE_BYTES as usize)},"image":null});
    assert!(unpack(&serde_json::to_vec(&bad).unwrap(), scratch.path()).is_err());
}

#[cfg(unix)]
fn fake_docker(scratch: &Scratch, hold: bool) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let executable = scratch.join("docker");
    let script = format!(
        r#"#!/usr/bin/python3
import json,os,sys
from pathlib import Path
root=Path({root:?})
a=sys.argv[1:]
assert a[:2]==['--host','unix:///var/run/docker.sock']
assert a[2]=='--config' and Path(a[3]).is_dir()
assert set(os.environ)<=set(['PATH','LANG'])
a=a[4:]
(root/'calls').open('a').write(a[0]+'\n')
if a[0]=='image': print(json.dumps({{'id':'sha256:'+'a'*64,'volumes':None}}))
elif a[0]=='create':
 print('b'*64)
 if (root/'fail-create').exists(): sys.exit(1)
elif a[0]=='start':
 p=json.load(sys.stdin)
 if (root/'fail-packet').exists(): print('invalid');sys.exit(1)
 assert p['suffix']=='csv' and 'launcher.py' in p['helpers']
 (root/'started').touch()
 if {hold}:
  import select
  with (root/'release').open() as gate: select.select([gate],[],[]);gate.read()
 print(json.dumps({{'preview':{{'status':'passed','kind':'table','columns':['id'],'rows':[['001']]}},'image':None}}))
elif a[0]=='rm':
 assert a==['rm','--force','b'*64]
 (root/'cleaned').touch()
else: sys.exit(2)
"#,
        root = scratch.path().to_string_lossy(),
        hold = if hold { "True" } else { "False" }
    );
    fs::write(&executable, script).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    executable
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_process_routes_resolve_create_attach_and_owned_cleanup() {
    let scratch = Scratch::new("container-preview-process");
    let runtime = runtime(fake_docker(&scratch, false));
    let service = super::super::PreviewService::with_settings(
        scratch.join("cache"),
        super::super::sandbox::Settings::Docker(runtime.settings),
    )
    .unwrap();
    let value = service
        .preview(super::super::tests::original(), PreviewOptions::default())
        .await
        .unwrap();
    assert_eq!(value["rows"][0][0], "001");
    assert!(scratch.join("cleaned").exists());
    let cached = service
        .preview(super::super::tests::original(), PreviewOptions::default())
        .await
        .unwrap();
    assert_eq!(value, cached);
    assert_eq!(
        fs::read_to_string(scratch.join("calls")).unwrap(),
        "image\ncreate\nstart\nrm\nimage\n"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnection_retains_admission_and_reaps_daemon_work() {
    let scratch = Scratch::new("container-preview-cancel");
    let gate = scratch.join("release");
    let status = std::process::Command::new("/usr/bin/mkfifo")
        .arg(&gate)
        .status()
        .unwrap();
    assert!(status.success());
    let runtime = runtime(fake_docker(&scratch, true));
    let service = super::super::PreviewService::with_settings(
        scratch.join("cache"),
        super::super::sandbox::Settings::Docker(runtime.settings),
    )
    .unwrap();
    let clone = service.clone();
    let request = tokio::spawn(async move {
        clone
            .preview(super::super::tests::original(), PreviewOptions::default())
            .await
    });
    wait::until("container is attached", || scratch.join("started").exists());
    request.abort();
    let _cancelled = request.await;
    let other = service.admit().unwrap();
    assert_eq!(service.admit().err().unwrap().body.code, "preview_busy");
    fs::write(&gate, b"release").unwrap();
    wait::until("container is removed", || scratch.join("cleaned").exists());
    drop(other);
    wait::until("all conversion slots returned", || {
        service.slots.available_permits() == 2
    });
    assert!(service.admit().is_ok());
}

#[cfg(unix)]
#[tokio::test]
async fn container_transport_executes_native_csv_without_host_mount_permissions() {
    let scratch = Scratch::new("container-native-csv");
    let source = scratch.join("source.csv");
    fs::write(&source, b"account,value\n001,=1+1\n").unwrap();
    let original = fs::read(&source).unwrap();
    let args = [
        "-I".into(),
        "-c".into(),
        BOOTSTRAP.into(),
        scratch.join("private").into_os_string(),
        scratch.join("derived").into_os_string(),
    ];
    let (status, packet) = run_input(
        Path::new("/usr/bin/python3"),
        &args,
        CONVERSION_TIMEOUT,
        Some(payload(&source, &PreviewOptions::default()).unwrap()),
        PACKET_BYTES,
    )
    .await
    .unwrap();
    assert!(status.success());
    let result = unpack(&packet, scratch.path()).unwrap();
    assert_eq!(result["rows"][0][0], "001");
    assert_eq!(result["rows"][0][1], "=1+1");
    assert_eq!(fs::read(source).unwrap(), original);
}

#[cfg(unix)]
#[tokio::test]
async fn unavailable_or_volume_bearing_images_never_create_or_fallback() {
    use std::os::unix::fs::PermissionsExt as _;
    for volumes in [json!({"/data":{}}), json!("unexpected")] {
        let scratch = Scratch::new("container-image-refused");
        let executable = scratch.join("docker");
        let metadata = json!({"id":format!("sha256:{}", "a".repeat(64)), "volumes":volumes});
        fs::write(
            &executable,
            format!("#!/bin/sh\nprintf '%s' '{}'\n", metadata),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let settings = runtime(executable).settings;
        assert_eq!(
            settings.runtime().await.err().unwrap().body.code,
            "preview_unavailable"
        );
    }
    let scratch = Scratch::new("container-image-missing");
    let mut settings = runtime(scratch.join("missing-docker")).settings;
    assert_eq!(
        settings.runtime().await.err().unwrap().body.code,
        "preview_unavailable"
    );
    settings.image = "untrusted:mutable".into();
    assert_eq!(
        settings.runtime().await.err().unwrap().body.code,
        "preview_unavailable"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn create_failure_never_removes_an_unowned_allocation() {
    let scratch = Scratch::new("container-create-failure");
    fs::write(scratch.join("fail-create"), b"fail").unwrap();
    let runtime = runtime(fake_docker(&scratch, false));
    let service = super::super::PreviewService::with_settings(
        scratch.join("cache"),
        super::super::sandbox::Settings::Docker(runtime.settings),
    )
    .unwrap();
    let error = service
        .preview(super::super::tests::original(), PreviewOptions::default())
        .await
        .unwrap_err();
    assert_eq!(error.body.code, "preview_unavailable");
    assert_eq!(
        fs::read_to_string(scratch.join("calls")).unwrap(),
        "image\ncreate\n"
    );
    assert!(!scratch.join("cleaned").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn invalid_packet_still_removes_the_owned_container_and_never_caches() {
    let scratch = Scratch::new("container-packet-failure");
    fs::write(scratch.join("fail-packet"), b"fail").unwrap();
    let runtime = runtime(fake_docker(&scratch, false));
    let service = super::super::PreviewService::with_settings(
        scratch.join("cache"),
        super::super::sandbox::Settings::Docker(runtime.settings),
    )
    .unwrap();
    let error = service
        .preview(super::super::tests::original(), PreviewOptions::default())
        .await
        .unwrap_err();
    assert_eq!(error.body.code, "preview_unavailable");
    assert!(scratch.join("cleaned").exists());
    assert!(fs::read_dir(&service.root)
        .unwrap()
        .all(|entry| entry.unwrap().file_name() == ".lock"));
}
