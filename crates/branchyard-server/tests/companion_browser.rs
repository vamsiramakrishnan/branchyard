//! The companion page in headless Chromium against a real server: it pairs
//! from a link, sees a branch appear and settle over the event stream, and
//! sends a follow-up from the branch page. Driven by Playwright
//! (`companion_browser.js`); skipped, saying so, when Node or Playwright's
//! module is not installed. `BY_TEST_PLAYWRIGHT` names the module's
//! directory; otherwise `$(npm root -g)/playwright` is tried, with
//! `PLAYWRIGHT_BROWSERS_PATH` (default `/opt/pw-browsers` when present).

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use branchyard_server::companion::{link, store};
use common::{run, task, Fixture, Server};

fn playwright() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("BY_TEST_PLAYWRIGHT") {
        return Some(PathBuf::from(path)).filter(|p| p.is_dir());
    }
    let out = Command::new("npm").args(["root", "-g"]).output().ok()?;
    let root = String::from_utf8(out.stdout).ok()?;
    Some(PathBuf::from(root.trim()).join("playwright")).filter(|p| p.is_dir())
}

#[test]
fn the_page_pairs_follows_the_stream_and_sends() {
    let Some(module) = playwright() else {
        eprintln!("skipped: no Playwright module (set BY_TEST_PLAYWRIGHT)");
        return;
    };
    let f = Fixture::new();
    let mut config = f.config();
    config.app.enabled = true;
    // No push service is reachable here; the page's in-page notices remain.
    config.app.push = false;
    let server = Server::start(config.clone());
    let companion = store::open(&config).unwrap();
    let request = link::LinkRequest {
        name: Some("browser".into()),
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

    let browsers = std::env::var("PLAYWRIGHT_BROWSERS_PATH").ok().or_else(|| {
        std::path::Path::new("/opt/pw-browsers")
            .is_dir()
            .then(|| "/opt/pw-browsers".to_owned())
    });
    let mut command = Command::new("node");
    command
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/companion_browser.js"
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
    expect("LOADED");
    // A branch made elsewhere appears on the open page.
    let op = run(&server.client(), &task("WRITE a.txt=1", "first"));
    assert!(op.state.is_terminal());
    writeln!(stdin, "go").unwrap();
    expect("SEEN first");
    expect("SENT");
    expect("DONE");
    assert!(child.wait().unwrap().success());

    // The send went through the API as the paired principal.
    let repo = server.client().repo("app");
    let branch = repo.branch("first").unwrap();
    assert_eq!(branch.turns, 2);
    let events = repo.events("first", 0).unwrap();
    assert!(events.events.iter().any(|e| matches!(
        &e.activity,
        branchyard::Activity::Prompt(p) if p.contains("WRITE b.txt=2")
    )));
}
