//! PR integration with real engine candidates and fake forge executables.

#![cfg(unix)]
#![allow(clippy::unwrap_used)] // tests: a panic reports the failed check

mod common;

use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use branchyard::{Policy, PullRequestActivity, PullRequestObservation, PullRequestRef, Yard};
use branchyard_server::console_pr::{Action, AuthorizedBranch, Error, Publish, Service};
use common::{fake_agent, Fixture};
use serde_json::json;

#[tokio::test]
async fn publication_reuses_checked_cli_and_refresh_includes_review_details() {
    let f = Fixture::new();
    let yard = Yard::open(&f.root).unwrap();
    let branch = yard
        .task("WRITE report.txt=reviewed")
        .name("review")
        .harness("gemini-cli")
        .command([fake_agent().display().to_string()])
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    branch
        .record_pull_request(PullRequestActivity::Opened(PullRequestRef {
            number: 17,
            url: "https://github.com/example/project/pull/17".into(),
            head: "by/review".into(),
            base: Some("main".into()),
            draft: false,
        }))
        .unwrap();
    branch
        .record_pull_request(PullRequestActivity::Observed(PullRequestObservation {
            number: 17,
            state: "open".into(),
            review_decision: Some("approved".into()),
            ..PullRequestObservation::default()
        }))
        .unwrap();
    let by = f.dir.join("fake-by");
    std::fs::write(&by,"#!/bin/sh\nprintf '%s\\n' \"$@\" > pr-arguments\nprintf '%s' '{\"pull_request\":{\"number\":17}}'\n").unwrap();
    std::fs::set_permissions(&by, std::fs::Permissions::from_mode(0o755)).unwrap();
    let gh = f.dir.join("fake-gh");
    std::fs::write(&gh,"#!/bin/sh\nprintf '%s\\n' \"$@\" > gh-arguments\nprintf '%s' '{\"comments\":[{\"body\":\"Check sources\"}],\"reviews\":[{\"state\":\"APPROVED\"}],\"statusCheckRollup\":[]}'\n").unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let context = AuthorizedBranch::checked(yard, "review", Action::Publish, |_| Ok(())).unwrap();
    let service = Service::new(by).with_gh(gh);
    let observed = service.inspect(&context).unwrap();
    assert_eq!(observed.pull_request.unwrap().number, 17);
    assert_eq!(
        observed.observation.unwrap().review_decision.as_deref(),
        Some("approved")
    );
    let result = service
        .publish(
            &context,
            &Publish {
                title: Some("--force".into()),
                base: Some("main".into()),
                draft: true,
            },
        )
        .await
        .unwrap();
    assert_eq!(result["pull_request"]["number"], 17);
    let argv = std::fs::read_to_string(f.root.join("pr-arguments")).unwrap();
    assert_eq!(
        argv,
        "pr\n--json\n--title=--force\n--base=main\n--draft\n--\nreview\n"
    );
    let refreshed = service.refresh(&context).await.unwrap();
    assert_eq!(
        refreshed.forge.unwrap()["comments"][0]["body"],
        "Check sources"
    );
    let gh_args = std::fs::read_to_string(f.root.join("gh-arguments")).unwrap();
    assert!(gh_args.starts_with("pr\nview\n17\n--json\n"));
}

#[tokio::test]
async fn revoked_members_and_read_only_contexts_cannot_publish() {
    let f = Fixture::new();
    let yard = Yard::open(&f.root).unwrap();
    yard.task("WRITE report.txt=reviewed")
        .name("review")
        .harness("gemini-cli")
        .command([fake_agent().display().to_string()])
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    let active = Arc::new(AtomicBool::new(true));
    let check = active.clone();
    let context = AuthorizedBranch::checked(yard.clone(), "review", Action::Publish, move |_| {
        if check.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(Error::denied("membership revoked"))
        }
    })
    .unwrap();
    let service = Service::new(f.dir.join("must-not-start"));
    active.store(false, Ordering::SeqCst);
    assert_eq!(service.inspect(&context).unwrap_err().code, "denied");
    assert_eq!(
        service
            .publish(&context, &Publish::default())
            .await
            .unwrap_err()
            .code,
        "denied"
    );
    let read = AuthorizedBranch::checked(yard, "review", Action::Read, |_| Ok(())).unwrap();
    assert_eq!(
        service
            .publish(&read, &Publish::default())
            .await
            .unwrap_err()
            .code,
        "denied"
    );
}

#[tokio::test]
async fn candidate_changes_and_invalid_controls_fail_before_side_effects() {
    let f = Fixture::new();
    let yard = Yard::open(&f.root).unwrap();
    let branch = yard
        .task("WRITE report.txt=one")
        .name("review")
        .harness("gemini-cli")
        .command([fake_agent().display().to_string()])
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    let context =
        AuthorizedBranch::checked(yard.clone(), "review", Action::Publish, |_| Ok(())).unwrap();
    let service = Service::new(f.dir.join("must-not-start"));
    assert_eq!(
        service
            .publish(
                &context,
                &Publish {
                    title: Some("bad\ntitle".into()),
                    ..Publish::default()
                }
            )
            .await
            .unwrap_err()
            .code,
        "invalid_pr_request"
    );
    branch
        .send("WRITE report.txt=two", branchyard::TaskOptions::default())
        .unwrap();
    assert_eq!(
        service
            .publish(&context, &Publish::default())
            .await
            .unwrap_err()
            .code,
        "candidate_changed"
    );
    let missing = AuthorizedBranch::checked(yard, "missing", Action::Read, |_| Ok(()));
    assert!(missing.is_err());
}

#[tokio::test]
async fn malformed_forge_output_is_uncertain_and_deadlines_stop_the_process_group() {
    let f = Fixture::new();
    let yard = Yard::open(&f.root).unwrap();
    yard.task("WRITE report.txt=one")
        .name("review")
        .harness("gemini-cli")
        .command([fake_agent().display().to_string()])
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    let context = AuthorizedBranch::checked(yard, "review", Action::Publish, |_| Ok(())).unwrap();
    let by = f.dir.join("fake-by");
    std::fs::write(&by, "#!/bin/sh\nprintf 'not json'\n").unwrap();
    std::fs::set_permissions(&by, std::fs::Permissions::from_mode(0o755)).unwrap();
    let error = Service::new(by.clone())
        .publish(&context, &Publish::default())
        .await
        .unwrap_err();
    assert_eq!(error.code, "pr_protocol");
    assert!(error.unknown_effect);
    std::fs::write(
        &by,
        "#!/bin/sh\nexec python3 -c 'print(\"x\" * (3 * 1024 * 1024))'\n",
    )
    .unwrap();
    let error = Service::new(by.clone())
        .publish(&context, &Publish::default())
        .await
        .unwrap_err();
    assert_eq!(error.code, "pr_output_limit");
    assert!(error.unknown_effect);
    std::fs::write(
        &by,
        "#!/bin/sh\nexec python3 -c 'import signal; signal.pause()'\n",
    )
    .unwrap();
    let error = Service::new(by)
        .with_timeout(Duration::ZERO)
        .publish(&context, &Publish::default())
        .await
        .unwrap_err();
    assert_eq!(error.code, "pr_timeout");
    assert!(error.unknown_effect);
    assert!(serde_json::from_value::<Publish>(json!({"force":true}))
        .unwrap_err()
        .to_string()
        .contains("unknown field"));
}

#[test]
fn revocation_while_publishing_withholds_the_result_without_claiming_no_effect() {
    let f = Fixture::new();
    let yard = Yard::open(&f.root).unwrap();
    yard.task("WRITE report.txt=one")
        .name("review")
        .harness("gemini-cli")
        .command([fake_agent().display().to_string()])
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    let active = Arc::new(AtomicBool::new(true));
    let check = active.clone();
    let context = AuthorizedBranch::checked(yard, "review", Action::Publish, move |_| {
        if check.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(Error::denied("membership revoked"))
        }
    })
    .unwrap();
    let by = f.dir.join("fake-by");
    std::fs::write(&by,"#!/usr/bin/python3\nimport os,signal,sys\nfrom pathlib import Path\ndef done(*_):\n print('{}',flush=True)\n sys.exit(0)\nsignal.signal(signal.SIGUSR1,done)\nPath('pr-ready').write_text(str(os.getpid()))\nsignal.pause()\n").unwrap();
    std::fs::set_permissions(&by, std::fs::Permissions::from_mode(0o755)).unwrap();
    let thread = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(Service::new(by).publish(&context, &Publish::default()))
    });
    let marker = f.root.join("pr-ready");
    let pid =
        branchyard_testkit::wait::until("the PR command to reach the revocation boundary", || {
            std::fs::read_to_string(&marker)
                .and_then(|text| text.parse::<u32>().map_err(std::io::Error::other))
        });
    active.store(false, Ordering::SeqCst);
    let status = std::process::Command::new("/bin/kill")
        .args(["-USR1", &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let error = thread.join().unwrap().unwrap_err();
    assert_eq!(error.code, "denied");
    assert!(error.unknown_effect);
}
