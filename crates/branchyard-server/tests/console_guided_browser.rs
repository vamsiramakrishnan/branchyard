//! The console's guided start and parallel sessions in headless Chromium
//! against a real server that requires operator execution presets: a
//! paired owner uploads sources from the guided form (one upload meeting a
//! real revision conflict), sees each environment with its readiness, and
//! starts two writer sessions on the local preset with different sources
//! and drafts that each read only their own inputs and publish only their
//! own outputs; continues one so its second turn publishes again; accepts
//! an output after a real revision conflict; starts on a Substrate preset
//! whose cluster is closed and one whose key is missing, both admitted and
//! failing with the server's projected error and a Try again that restores
//! the draft; is refused while the workspace is archived and starts the
//! same draft after Restore; starts an Antigravity writer only with the
//! tool acknowledgement; and starts one session with the keyboard alone. A
//! viewer and a reviewer, made members through the API and signed in with
//! their own tokens, see what their roles allow; the reviewer starts and
//! continues a review. The form and the workspace fit 390 pixels. Driven by
//! Playwright (`console_guided_browser.js`) with documents from
//! `tools/console_fixtures.py`, the fake ACP agent as `claude-code-acp`,
//! and a fake Antigravity stream; skipped, saying so, when Node or
//! Playwright's module is not installed. `BY_TEST_PLAYWRIGHT` names the
//! module's directory; otherwise `$(npm root -g)/playwright` is tried, with
//! `PLAYWRIGHT_BROWSERS_PATH` (default `/opt/pw-browsers` when present).
//! `BY_SCREENSHOTS=DIR` saves the guided form and a workspace with two
//! sessions at 1440 by 900 and 390 by 844, light and dark.
#![cfg(unix)]

mod common;

use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use branchyard_server::companion::{link, store};
use branchyard_server::config::{Principal, Token};
use common::{fake_agent, Fixture, Server, TOKEN};

const VIEWER_TOKEN: &str = "console-guided-viewer-token";
const REVIEWER_TOKEN: &str = "console-guided-reviewer-token";

/// Copies every input its session was given into `outputs/` under its
/// title, lists them in `outputs/artifacts.json`, and ends with the summary
/// a Collect outputs session owes.
const COLLECT: &str = r#"import glob, hashlib, json, os, shutil
os.makedirs('outputs', exist_ok=True)
files = []
for index in glob.glob('.console-inputs/*/*/INDEX.md'):
    with open(index) as listed:
        inputs = json.load(listed)['inputs']
    for item in inputs:
        name = os.path.basename(item['title'])
        shutil.copyfile(item['path'], os.path.join('outputs', name))
        with open(os.path.join('outputs', name), 'rb') as copied:
            digest = hashlib.sha256(copied.read()).hexdigest()
        files.append({'path': name, 'media_type': item['media_type'], 'sha256': digest})
with open('outputs/artifacts.json', 'w') as manifest:
    json.dump({'schema_version': 1, 'files': files}, manifest)
print('copied ' + ', '.join(sorted(f['path'] for f in files)))
print('\n```json\n' + json.dumps({'summary': 'copied %d inputs' % len(files)}) + '\n```')
"#;

/// Antigravity's stream-json, answering every turn with SUCCESS.
const FAKE_AGY: &str = r#"#!/usr/bin/python3
import json, sys
session = sys.argv[sys.argv.index('--conversation') + 1] if '--conversation' in sys.argv else 'hermetic-agy-session'
print(json.dumps({'event':'init','conversation_id':session,'init':{'cwd':'.','tools':[]}}),flush=True)
for number, line in enumerate(sys.stdin):
    json.loads(line)
    print(json.dumps({'event':'step_update','step_update':{'conversation_id':session,'step_index':number,'step_type':'user_input','state':'DONE'}}),flush=True)
    print(json.dumps({'event':'result','result':{'conversation_id':session,'status':'SUCCESS','response':'Checked evidence','num_turns':number+1}}),flush=True)
"#;

fn playwright() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("BY_TEST_PLAYWRIGHT") {
        return Some(PathBuf::from(path)).filter(|p| p.is_dir());
    }
    let out = Command::new("npm").args(["root", "-g"]).output().ok()?;
    let root = String::from_utf8(out.stdout).ok()?;
    Some(PathBuf::from(root.trim()).join("playwright")).filter(|p| p.is_dir())
}

#[test]
fn the_guided_start_runs_parallel_sessions_in_operator_environments_in_the_browser() {
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
    let collect = fixtures.join("collect.py");
    std::fs::write(&collect, COLLECT).unwrap();
    let agy = f.dir.join("fake-agy");
    std::fs::write(&agy, FAKE_AGY).unwrap();
    std::fs::set_permissions(&agy, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::create_dir_all(f.dir.join("substrate")).unwrap();
    branchyard_bridge::Signer::write(&f.dir.join("substrate/live.key")).unwrap();

    let closed = "http://127.0.0.1:1";
    let substrate = |key: &str| {
        serde_json::json!({"kind": "substrate", "endpoint": closed, "router": closed,
            "template": "harnesses", "key": key})
    };
    let models = serde_json::json!({"harness": "claude-code-acp",
        "models": ["fake-fast", "fake-deep"], "default_model": "fake-fast"});
    let path = f.dir.join("server.json");
    std::fs::write(
        &path,
        serde_json::json!({
            "data_dir": f.data,
            "repos": { "app": f.root },
            "tokens": [{ "name": "tester", "token": TOKEN }],
            "app": { "push": false },
            "managed_knowledge": { "enabled": true },
            "allow_unapproved_tools": true,
            "allow_providers": ["substrate"],
            "require_workspace_execution_presets": true,
            "execution_presets": [
                {"id": "local", "label": "Local workers",
                 "description": "Runs on this Branchyard server with its installed harnesses.",
                 "provider": {"kind": "local"},
                 "harnesses": [models, {"harness": "antigravity"}]},
                {"id": "substrate-closed", "label": "Substrate, cluster closed",
                 "description": "Runs each session in a Substrate actor; this cluster does not answer.",
                 "provider": substrate("substrate/live.key"), "harnesses": [models]},
                {"id": "substrate-missing", "label": "Substrate, key missing",
                 "description": "Runs each session in a Substrate actor; its bridge key is missing here.",
                 "provider": substrate("substrate/missing.key"), "harnesses": [models]}
            ],
        })
        .to_string(),
    )
    .unwrap();
    let mut config =
        branchyard_server::cli::resolve(&["--config".into(), path.display().to_string()]).unwrap();
    config.listen = "127.0.0.1:0".parse().unwrap();
    config.harness_commands.insert(
        "claude-code-acp".into(),
        vec![fake_agent().display().to_string()],
    );
    config
        .harness_commands
        .insert("antigravity".into(), vec![agy.display().to_string()]);
    config.shutdown_grace = Duration::from_secs(10);
    config.poll_interval = Duration::from_millis(100);
    config.log_requests = false;
    config.inventory_source = Some(branchyard_server::ops::InventorySource(Arc::new(|| None)));
    for (name, secret) in [("viewer", VIEWER_TOKEN), ("reviewer", REVIEWER_TOKEN)] {
        config.tokens.push(Token {
            name: name.into(),
            secret: secret.into(),
        });
        let mut principal = Principal::default_for(name);
        principal.scopes = ["read".to_owned(), "run".to_owned()].into_iter().collect();
        config.principals.insert(name.into(), principal);
    }

    let server = Server::start(config.clone());
    let companion = store::open(&config).unwrap();
    let request = link::LinkRequest {
        name: Some("owner-browser".into()),
        tenant: "default".into(),
        scopes: vec!["read".into(), "run".into(), "merge".into(), "admin".into()],
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
    // What the page and the session JSON must never show of the Substrate
    // presets' configuration.
    let configured = serde_json::json!([
        f.dir.display().to_string(),
        "substrate/live.key",
        "substrate/missing.key",
        "127.0.0.1:1",
    ]);

    let browsers = std::env::var("PLAYWRIGHT_BROWSERS_PATH").ok().or_else(|| {
        std::path::Path::new("/opt/pw-browsers")
            .is_dir()
            .then(|| "/opt/pw-browsers".to_owned())
    });
    let mut command = Command::new("node");
    command
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/console_guided_browser.js"
        ))
        .env("PLAYWRIGHT", &module)
        .env("BY_URL", server.url())
        .env("BY_CODE", &code)
        .env("BY_VIEWER_TOKEN", VIEWER_TOKEN)
        .env("BY_REVIEWER_TOKEN", REVIEWER_TOKEN)
        .env("BY_FIXTURES", &fixtures)
        .env("BY_COLLECT", &collect)
        .env("BY_CONFIGURED", configured.to_string())
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
        "SOURCES",
        "ENVIRONMENTS",
        "PARALLEL",
        "FOLLOWUP",
        "ACCEPTED",
        "REMOTE",
        "ARCHIVED",
        "ANTIGRAVITY",
        "ROLES",
        "KEYBOARD",
        "PHONE",
        "DONE",
    ] {
        expect(step);
    }
    assert!(child.wait().unwrap().success());
}
