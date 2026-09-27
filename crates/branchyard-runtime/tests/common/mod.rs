#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use branchyard_harness::acp::Acp;
use branchyard_harness::{Driver, Open, SessionMode};
use branchyard_runtime::{Environment, Session};

pub const WAIT: Duration = Duration::from_secs(10);

pub fn agent() -> Box<dyn Driver> {
    Box::new(Acp::new(vec![env!("CARGO_BIN_EXE_fake-acp-agent").into()]))
}

/// A fresh directory for one test, with a `home` inside it.
pub fn workdir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("home")).unwrap();
    dir
}

pub fn open(dir: &Path, mode: SessionMode) -> Open {
    Open {
        mode,
        cwd: dir.display().to_string(),
        model: None,
        mcp_servers: Vec::new(),
    }
}

/// A ready session on the fake agent, with a transcript in `dir`.
pub fn start_with(dir: &Path, mode: SessionMode, env: &Environment) -> Session {
    let mut session = Session::start(
        agent(),
        open(dir, mode),
        env,
        Some(&dir.join("transcript.jsonl")),
    )
    .unwrap();
    session.wait_ready(WAIT).unwrap();
    session
}

pub fn start(name: &str) -> (Session, PathBuf) {
    let dir = workdir(name);
    let session = start_with(
        &dir,
        SessionMode::Fresh,
        &Environment::new(dir.join("home")),
    );
    (session, dir)
}

/// True while `pid` is a live, non-zombie process.
pub fn alive(pid: u32) -> bool {
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    let stat = stat.trim();
    !stat.is_empty() && !stat.starts_with('Z')
}

pub fn background_pid(text: &str) -> u32 {
    text.strip_prefix("background pid ")
        .and_then(|pid| pid.trim().parse().ok())
        .unwrap_or_else(|| panic!("no background pid in {text:?}"))
}
