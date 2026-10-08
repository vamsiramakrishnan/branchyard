//! What each error says. Refusals reach a person through Branchyard's
//! messages, so every variant's text names what went wrong and where.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
use std::error::Error as _;
use std::io;
use std::os::unix::process::ExitStatusExt as _;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::time::Duration;

use branchyard_workspace::{BranchName, Commit, GitError, IntegrationError, InvalidBranchName};

#[test]
fn every_integration_error_names_its_cause() {
    let commit = |c: &str| Commit(c.repeat(40));
    let cases: Vec<(IntegrationError, &str)> = vec![
        (
            IntegrationError::TargetMoved {
                expected: commit("a"),
                actual: Some(commit("b")),
            },
            "target moved from aaaa",
        ),
        (
            IntegrationError::TargetMoved {
                expected: commit("a"),
                actual: None,
            },
            "no longer exists",
        ),
        (
            IntegrationError::Conflict {
                files: vec!["x.rs".into(), "y.rs".into()],
            },
            "merge conflicts in x.rs, y.rs",
        ),
        (
            IntegrationError::ConflictWith {
                candidate: "by/b".into(),
                merged: Vec::new(),
                files: vec!["x.rs".into()],
            },
            "by/b conflicts with the target in x.rs",
        ),
        (
            IntegrationError::ConflictWith {
                candidate: "by/c".into(),
                merged: vec!["by/a".into(), "by/b".into()],
                files: vec!["x.rs".into()],
            },
            "by/c conflicts with the target after merging by/a, by/b in x.rs",
        ),
        (
            IntegrationError::CheckFailed {
                status: ExitStatus::from_raw(3 << 8),
                output_tail: String::new(),
            },
            "check failed: exit status: 3",
        ),
        (
            IntegrationError::CheckTimedOut {
                timeout: Duration::from_secs(2),
                output_tail: String::new(),
            },
            "check timed out after 2s",
        ),
        (
            IntegrationError::CheckNotStarted(io::Error::other("no such program")),
            "check could not start: no such program",
        ),
        (
            IntegrationError::CheckStopped {
                index: 1,
                error: Box::new(IntegrationError::CheckFailed {
                    status: ExitStatus::from_raw(3 << 8),
                    output_tail: String::new(),
                }),
            },
            "check 2: check failed: exit status: 3",
        ),
        (
            IntegrationError::DirtyTarget {
                worktree: PathBuf::from("/w"),
            },
            "uncommitted changes in /w",
        ),
        (
            IntegrationError::AlreadyIntegrated,
            "already contained in the target",
        ),
        (
            IntegrationError::InvalidCandidate("forged".into()),
            "invalid candidate: forged",
        ),
        (
            IntegrationError::Git(GitError::InvalidRef("-x".into())),
            "\"-x\" is not a valid local branch name",
        ),
    ];
    for (error, needed) in cases {
        let text = error.to_string();
        assert!(text.contains(needed), "{needed:?} missing from {text}");
    }
    assert!(IntegrationError::CheckNotStarted(io::Error::other("x"))
        .source()
        .is_some());
    assert!(IntegrationError::Git(GitError::Parse("x".into()))
        .source()
        .is_some());
    assert!(IntegrationError::AlreadyIntegrated.source().is_none());
}

#[test]
fn every_git_error_names_its_cause() {
    let name: BranchName = "kid".parse().unwrap();
    let cases: Vec<(GitError, &str)> = vec![
        (
            GitError::Spawn(io::Error::other("no git")),
            "could not run git: no git",
        ),
        (
            GitError::Failed {
                args: vec!["merge".into(), "x".into()],
                code: Some(128),
                stderr: "fatal: nope\n".into(),
            },
            "git merge x failed with exit code 128: fatal: nope",
        ),
        (
            GitError::Failed {
                args: vec!["gc".into()],
                code: None,
                stderr: " ".into(),
            },
            "git gc failed",
        ),
        (
            GitError::NotAWorkTree(PathBuf::from("/nowhere")),
            "/nowhere is not a git working tree",
        ),
        (
            GitError::InvalidRevision("zz".into()),
            "\"zz\" does not name a commit",
        ),
        (
            GitError::BranchExists(name.clone()),
            "branch by/kid already exists",
        ),
        (
            GitError::NotOnBranch {
                expected: "by/kid".into(),
                actual: None,
            },
            "on a detached commit instead of by/kid",
        ),
        (
            GitError::NotOnBranch {
                expected: "by/kid".into(),
                actual: Some("main".into()),
            },
            "on main instead of by/kid",
        ),
        (
            GitError::MissingBase(name.clone()),
            "no base commit recorded for by/kid",
        ),
        (
            GitError::NotDescendant {
                base: "b".into(),
                head: "h".into(),
            },
            "h does not descend from base b",
        ),
        (GitError::Io(io::Error::other("disk full")), "disk full"),
        (GitError::Parse("odd".into()), "unexpected git output: odd"),
    ];
    for (error, needed) in cases {
        let text = error.to_string();
        assert!(text.contains(needed), "{needed:?} missing from {text}");
    }
    assert!(GitError::Io(io::Error::other("x")).source().is_some());
    assert!(GitError::Parse("x".into()).source().is_none());
    let from: GitError = io::Error::other("x").into();
    assert!(matches!(from, GitError::Io(_)));
}

#[test]
fn a_rejected_branch_name_says_what_is_allowed() {
    let error = "Not Valid".parse::<BranchName>().unwrap_err();
    assert_eq!(error, InvalidBranchName("Not Valid".into()));
    assert!(error.to_string().contains("lowercase segments"), "{error}");
    let name: BranchName = "a/b".parse().unwrap();
    assert_eq!(AsRef::<str>::as_ref(&name), "a/b");
}
