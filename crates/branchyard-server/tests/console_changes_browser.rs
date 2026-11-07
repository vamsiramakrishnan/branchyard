//! The console's Changes page in headless Chromium against a real server
//! and the fake ACP agent: a branch with a real diff (a modified file and
//! two new ones), opened from the board, filtered, walked with the keyboard
//! and marked viewed; a comment on a dragged range of lines and another on
//! one line, a pending tray, an overall note, and one review sent. The
//! branch must then have received exactly one prompt naming the files, the
//! lines and the code they quote, with one feedback record; the page read
//! the diff and the reviews once each and the review cost one write and one
//! read. A later turn that rewrites those lines raises the "new candidate"
//! banner, and reloading marks the comment on them addressed and the other
//! as sent. Driven by Playwright (`console_changes_browser.js`); skipped,
//! saying so, when Node or Playwright's module is not installed.
//! `BY_TEST_PLAYWRIGHT` names the module's directory; otherwise
//! `$(npm root -g)/playwright` is tried, with `PLAYWRIGHT_BROWSERS_PATH`
//! (default `/opt/pw-browsers` when present). `BY_SCREENSHOTS=DIR` saves the
//! page at 1440 by 900 and 390 by 844, light and dark.
#![cfg(unix)]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use branchyard::{Activity, BranchStatus, FeedbackSource};
use branchyard_client::api::{OperationState, SendRequest, TaskRequest};
use branchyard_client::new_key;
use branchyard_server::companion::{link, store};
use branchyard_testkit::wait;
use common::{await_operation, run, Fixture, Server};

fn task(prompt: &str, name: &str) -> TaskRequest {
    TaskRequest {
        isolated: true,
        ..common::task(prompt, name)
    }
}

fn playwright() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("BY_TEST_PLAYWRIGHT") {
        return Some(PathBuf::from(path)).filter(|p| p.is_dir());
    }
    let out = Command::new("npm").args(["root", "-g"]).output().ok()?;
    let root = String::from_utf8(out.stdout).ok()?;
    Some(PathBuf::from(root.trim()).join("playwright")).filter(|p| p.is_dir())
}

/// The first turn: a modified file and two new ones, written by the fake
/// agent's shell.
const FIRST: &str = r#"SH printf 'one\ntwo\n' > a.txt
SH printf '# Notes\n\nKeep it small.\n' > notes.md
SH mkdir -p src && printf 'fn main() {\n    let first = 1;\n    let second = 2;\n    let total = first + second;\n    println!("{total}");\n}\n' > src/main.rs"#;

/// The turn that answers the comment on lines 2 and 3: it renames both
/// variables, so lines 2 to 4 change and line 5 does not.
const SECOND: &str = r#"SH printf 'fn main() {\n    let a = 1;\n    let b = 2;\n    let total = a + b;\n    println!("{total}");\n}\n' > src/main.rs"#;

#[test]
fn the_changes_page_reviews_a_diff_and_sends_one_prompt() {
    let Some(module) = playwright() else {
        eprintln!("skipped: no Playwright module (set BY_TEST_PLAYWRIGHT)");
        return;
    };
    let f = Fixture::new();
    let mut config = f.config();
    config.app.enabled = true;
    config.app.push = false;
    let server = Server::start(config.clone());
    let companion = store::open(&config).unwrap();
    let request = link::LinkRequest {
        name: Some("changes-browser".into()),
        tenant: "default".into(),
        scopes: vec!["read".into(), "run".into(), "merge".into()],
        repos: None,
        ttl: None,
        code_ttl: None,
        public_url: None,
        qr: false,
    };
    let (_, code) = link::create(
        &config,
        companion.as_ref(),
        &request,
        branchyard_support::time::now_ms(),
    )
    .unwrap();

    let client = server.client();
    let repo = client.repo("app");
    let made = run(&client, &task(FIRST, "polish"));
    assert_eq!(made.state, OperationState::Succeeded, "{made:?}");
    let changes = repo.changes("polish").unwrap();
    assert_eq!(changes.files.len(), 3, "{changes:?}");
    let first_commit = changes.commit.clone().expect("a candidate");

    let browsers = std::env::var("PLAYWRIGHT_BROWSERS_PATH").ok().or_else(|| {
        std::path::Path::new("/opt/pw-browsers")
            .is_dir()
            .then(|| "/opt/pw-browsers".to_owned())
    });
    let mut command = Command::new("node");
    command
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/console_changes_browser.js"
        ))
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/companion/assets/changes.js"
        ))
        .env("PLAYWRIGHT", &module)
        .env("BY_URL", server.url())
        .env("BY_CODE", &code)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if let Some(path) = browsers {
        command.env("PLAYWRIGHT_BROWSERS_PATH", path);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            eprintln!("skipped: cannot run node: {e}");
            return;
        }
    };
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut expect = |want: &str| {
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        if line.trim() != want {
            let status = child.wait().unwrap();
            panic!("the page script said {line:?} (exit {status}), not {want}");
        }
    };
    for step in ["MODEL", "LOADED", "FILES", "COMMENTED", "SENT"] {
        expect(step);
    }

    // The branch received one prompt for the whole review: the files, the
    // lines and the code they quote, in `by review`'s format.
    let settled = wait_for_review_turn(&repo);
    let events = repo.events("polish", 0).unwrap().events;
    let reviews: Vec<&str> = events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Prompt(text) if text.contains("Review comments on this branch's changes") => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(reviews.len(), 1, "{reviews:?}");
    let prompt = reviews[0];
    for want in [
        "(2 comments on 1 file)",
        "Overall note: \"Please keep the diff small.\"",
        "File: src/main.rs\nLines: 2-3\nQuoted lines:\n>     let first = 1;\n>     let second = 2;\n\
         User comment: \"Name these better.\\nKeep them short.\"",
        "File: src/main.rs\nLine: 5\nQuoted lines:\n>     println!(\"{total}\");\n\
         User comment: \"Pad the total?\"",
        "Address each comment on this branch.",
    ] {
        assert!(prompt.contains(want), "{want:?} is not in {prompt}");
    }
    let feedback: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Feedback(f) => Some(f),
            _ => None,
        })
        .collect();
    assert_eq!(feedback.len(), 1, "one feedback record");
    assert_eq!(feedback[0].source, FeedbackSource::Review);
    assert_eq!(
        feedback[0].reference.as_deref(),
        Some(first_commit.as_str())
    );
    assert_eq!(feedback[0].items.len(), 3, "two comments and the note");
    assert!(!matches!(settled.status, BranchStatus::Running));
    writeln!(stdin, "go").unwrap();

    // The agent answers the first comment: lines 2 to 4 are rewritten.
    let second = repo
        .send(
            "polish",
            &SendRequest {
                prompt: SECOND.into(),
                ..SendRequest::default()
            },
            &new_key(),
        )
        .unwrap();
    let second = await_operation(&client, &second.id);
    assert_eq!(second.state, OperationState::Succeeded, "{second:?}");
    writeln!(stdin, "changed").unwrap();
    for step in ["BANNER", "ADDRESSED", "VIEWPORTS", "DONE"] {
        expect(step);
    }
    assert!(child.wait().unwrap().success());

    // What the page showed is what the server holds.
    let now = repo.changes("polish").unwrap().commit.unwrap();
    assert_ne!(now, first_commit);
    let held = repo.reviews("polish").unwrap();
    assert_eq!(held.reviews.len(), 1);
    let comments = &held.reviews[0].comments;
    assert_eq!(comments[0].addressed_in.as_deref(), Some(now.as_str()));
    assert_eq!(comments[1].addressed_in, None);
}

/// Wait for the turn the review started to end, and return the branch.
fn wait_for_review_turn(repo: &branchyard_client::Repo) -> branchyard::BranchInfo {
    wait::until("the review's turn to end", || {
        let prompted = repo
            .events("polish", 0)
            .map_err(|e| e.to_string())?
            .events
            .iter()
            .any(|e| matches!(&e.activity, Activity::Prompt(p) if p.contains("Review comments")));
        let info = repo.branch("polish").map_err(|e| e.to_string())?;
        match (prompted, &info.status) {
            (true, BranchStatus::Running) | (false, _) => Err("not yet".to_owned()),
            (true, _) => Ok(info),
        }
    })
}
