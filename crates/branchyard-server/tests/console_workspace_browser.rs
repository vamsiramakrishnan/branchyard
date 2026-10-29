//! The console's knowledge workspaces in headless Chromium against a real
//! server with managed knowledge on: a knowledge-only token loads the
//! console, a paired browser creates a project and a workspace, uploads
//! sources, runs two fake-agent sessions (one steered, cancelled and
//! resumed), finds four captured outputs whose previews the server does not
//! serve, downloads and accepts them, and the branch routes still work
//! beside it. Driven by Playwright (`console_workspace_browser.js`) with
//! documents from `tools/console_fixtures.py`; skipped, saying so, when
//! Node or Playwright's module is not installed. `BY_TEST_PLAYWRIGHT`
//! names the module's directory; otherwise `$(npm root -g)/playwright` is
//! tried, with `PLAYWRIGHT_BROWSERS_PATH` (default `/opt/pw-browsers` when
//! present). `BY_SCREENSHOTS=DIR` saves desktop and phone views.

mod common;

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use branchyard_client::api::OperationState;
use branchyard_server::companion::{link, store};
use branchyard_server::config::{Principal, Token};
use common::{fake_agent, run, task, Fixture, Server, TOKEN};

const KNOWLEDGE_TOKEN: &str = "console-knowledge-only-token";

fn playwright() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("BY_TEST_PLAYWRIGHT") {
        return Some(PathBuf::from(path)).filter(|p| p.is_dir());
    }
    let out = Command::new("npm").args(["root", "-g"]).output().ok()?;
    let root = String::from_utf8(out.stdout).ok()?;
    Some(PathBuf::from(root.trim()).join("playwright")).filter(|p| p.is_dir())
}

#[test]
fn a_knowledge_workspace_runs_sessions_and_accepts_outputs_in_the_browser() {
    let Some(module) = playwright() else {
        eprintln!("skipped: no Playwright module (set BY_TEST_PLAYWRIGHT)");
        return;
    };
    let f = Fixture::new();
    let fixtures = f.dir.join("fixtures");
    let generated = branchyard_testkit::hermetic(&mut Command::new("python3"))
        .arg("-I")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tools/console_fixtures.py"
        ))
        .arg("--out")
        .arg(&fixtures)
        .output()
        .unwrap_or_else(|e| panic!("python3 cannot run tools/console_fixtures.py: {e}"));
    assert!(
        generated.status.success(),
        "tools/console_fixtures.py could not write the document fixtures ({}): {}",
        generated.status,
        String::from_utf8_lossy(&generated.stderr)
    );

    let path = f.dir.join("server.json");
    std::fs::write(
        &path,
        serde_json::json!({
            "data_dir": f.data,
            "repos": { "app": f.root },
            "tokens": [{ "name": "tester", "token": TOKEN }],
            "app": { "push": false },
            "managed_knowledge": { "enabled": true },
        })
        .to_string(),
    )
    .unwrap();
    let mut config =
        branchyard_server::cli::resolve(&["--config".into(), path.display().to_string()]).unwrap();
    config.listen = "127.0.0.1:0".parse().unwrap();
    config.harness_commands.insert(
        "gemini-cli".into(),
        vec![fake_agent().display().to_string()],
    );
    config.shutdown_grace = Duration::from_secs(10);
    config.poll_interval = Duration::from_millis(100);
    config.log_requests = false;
    config.inventory_source = Some(branchyard_server::ops::InventorySource(Arc::new(|| None)));
    config.tokens.push(Token {
        name: "knowledge".into(),
        secret: KNOWLEDGE_TOKEN.into(),
    });
    let mut knowledge = Principal::default_for("knowledge");
    // The managed knowledge yard's name: this identity reaches knowledge
    // projects and no repository.
    knowledge.repos = Some(["__console_managed__".to_owned()].into_iter().collect());
    config.principals.insert("knowledge".into(), knowledge);

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
    let op = run(&server.client(), &task("WRITE a.txt=1", "first"));
    assert_eq!(op.state, OperationState::Succeeded, "{op:?}");

    let browsers = std::env::var("PLAYWRIGHT_BROWSERS_PATH").ok().or_else(|| {
        std::path::Path::new("/opt/pw-browsers")
            .is_dir()
            .then(|| "/opt/pw-browsers".to_owned())
    });
    let mut command = Command::new("node");
    command
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/console_workspace_browser.js"
        ))
        .env("PLAYWRIGHT", &module)
        .env("BY_URL", server.url())
        .env("BY_CODE", &code)
        .env("BY_KNOWLEDGE_TOKEN", KNOWLEDGE_TOKEN)
        .env("BY_FIXTURES", &fixtures)
        .stdin(Stdio::null())
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
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut expect = |want: &str| {
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        if line.trim() != want {
            let status = child.wait().unwrap();
            panic!("the page script said {line:?} (exit {status}), not {want}");
        }
    };
    for step in [
        "LOADED",
        "PROJECT",
        "WORKSPACE",
        "SOURCES",
        "SESSIONS",
        "OUTPUTS",
        "ACCEPTED",
        "DONE",
    ] {
        expect(step);
    }
    assert!(child.wait().unwrap().success());
}
