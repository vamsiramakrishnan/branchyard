// Derived from stablyai/orca src/main/automations/precheck-runner.ts and
// src/shared/automation-precheck.ts, at revision
// 280733273545f0b3eeedc1be54b14d406239030e.
// Copyright (c) 2026 Lovecast Inc. Licensed under the MIT License; the
// license text, which must accompany substantial portions of this code, is
// in vendor/orca/LICENSE.
// Modified for Branchyard: translated from TypeScript to Rust with
// threads instead of Node streams; only the local target is kept (Orca's
// SSH channel and Windows taskkill paths are left out); the command runs
// in a fresh detached worktree of the repository that is removed
// afterwards, with the trigger's name, run key and event file in its
// environment; a result is returned to record with the trigger's run, not
// sent over IPC. The constants, timeout normalization, pass rule and
// failure sentences are Orca's.

//! A trigger's precheck: a shell command run before firing, in a fresh
//! worktree. Exit 0 fires the trigger; any other exit, a timeout or a
//! command that cannot start skips the run with the reason (Orca's
//! `didAutomationPrecheckPass` and `formatAutomationPrecheckFailure`).
//! On a timeout the command's whole process group gets SIGTERM, then
//! SIGKILL two seconds later, as Orca's `killLocalPrecheckProcessTree`
//! does.

use branchyard_support::LockExt as _;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use branchyard_client::triggers::{Precheck, PrecheckResult};

pub const DEFAULT_TIMEOUT_SECONDS: u64 = 60;
pub const MAX_TIMEOUT_SECONDS: u64 = 600;
/// The tail of each stream a result keeps.
pub const MAX_OUTPUT_CHARS: usize = 4000;
/// How long a timed-out group has between SIGTERM and SIGKILL.
const FORCE_KILL_AFTER: Duration = Duration::from_secs(2);

/// Orca's `normalizeAutomationPrecheck`: a trimmed command, or none when
/// it is empty; the timeout within 1 to 600 seconds.
pub fn normalize(precheck: &Precheck) -> Option<Precheck> {
    let command = precheck.command.trim();
    if command.is_empty() {
        return None;
    }
    Some(Precheck {
        command: command.to_owned(),
        timeout_seconds: precheck.timeout_seconds.clamp(1, MAX_TIMEOUT_SECONDS),
    })
}

/// Orca's `formatAutomationPrecheckFailure`.
pub fn failure(result: &PrecheckResult) -> String {
    if result.timed_out {
        return format!(
            "Precheck timed out after {}s.",
            (result.duration_ms as f64 / 1000.0).round().max(1.0) as u64
        );
    }
    if let Some(error) = &result.error {
        return format!("Precheck failed: {error}");
    }
    match result.exit_code {
        Some(code) => format!("Precheck exited with code {code}."),
        None => "Precheck exited with code unknown.".into(),
    }
}

/// Orca's `appendTail`: keep the last [`MAX_OUTPUT_CHARS`] characters.
#[derive(Default)]
struct Tail {
    content: String,
    truncated: bool,
}

impl Tail {
    fn append(&mut self, chunk: &str) {
        self.content.push_str(chunk);
        let chars = self.content.chars().count();
        if chars > MAX_OUTPUT_CHARS {
            let skip = chars - MAX_OUTPUT_CHARS;
            let at = self
                .content
                .char_indices()
                .nth(skip)
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.content.drain(..at);
            self.truncated = true;
        }
    }
}

fn collect(mut stream: impl Read + Send + 'static) -> Arc<Mutex<Tail>> {
    let tail = Arc::new(Mutex::new(Tail::default()));
    let sink = tail.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        // Bytes of a character split across reads wait for the rest.
        let mut pending: Vec<u8> = Vec::new();
        loop {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    pending.extend_from_slice(&buf[..n]);
                    let valid = match std::str::from_utf8(&pending) {
                        Ok(_) => pending.len(),
                        Err(e) if e.error_len().is_none() => e.valid_up_to(),
                        Err(_) => pending.len(),
                    };
                    let text = String::from_utf8_lossy(&pending[..valid]).into_owned();
                    pending.drain(..valid);
                    sink.lock_recovering("sink").append(&text);
                }
            }
        }
    });
    tail
}

/// Run `command` with `sh -c` in `cwd`, with `env` added to this
/// process's environment, for at most `timeout`.
pub fn run(
    command: &str,
    timeout: Duration,
    cwd: &Path,
    env: &[(String, String)],
) -> PrecheckResult {
    let started = Instant::now();
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .envs(env.iter().map(|(k, v)| (k, v)));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own process group, so a timeout stops what it started too.
        cmd.process_group(0);
    }
    let result = |exit_code, timed_out, error: Option<String>, out: Option<(Tail, Tail)>| {
        let (stdout, stderr) = out.unwrap_or_default();
        PrecheckResult {
            command: command.to_owned(),
            exit_code,
            timed_out,
            duration_ms: started.elapsed().as_millis() as u64,
            stdout: stdout.content,
            stderr: stderr.content,
            stdout_truncated: stdout.truncated,
            stderr_truncated: stderr.truncated,
            error,
        }
    };
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return result(None, false, Some(e.to_string()), None),
    };
    let stdout = collect(child.stdout.take().expect("piped"));
    let stderr = collect(child.stderr.take().expect("piped"));
    let deadline = started + timeout;
    let mut timed_out = false;
    let mut force_at: Option<Instant> = None;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(e) => break Err(e),
        }
        let now = Instant::now();
        if !timed_out && now >= deadline {
            timed_out = true;
            #[cfg(unix)]
            branchyard_support::terminate_group(child.id());
            #[cfg(not(unix))]
            branchyard_support::best_effort("kill child", child.kill());
            force_at = Some(now + FORCE_KILL_AFTER);
        }
        if force_at.is_some_and(|at| now >= at) {
            #[cfg(unix)]
            branchyard_support::kill_group(child.id());
            branchyard_support::best_effort("kill child", child.kill());
            force_at = None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // The readers end when every process holding the pipes has; give
    // them a moment after the command itself ended.
    let settle = Instant::now() + Duration::from_millis(500);
    while (Arc::strong_count(&stdout) > 1 || Arc::strong_count(&stderr) > 1)
        && Instant::now() < settle
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    let take = |tail: &Arc<Mutex<Tail>>| std::mem::take(&mut *tail.lock_recovering("tail"));
    let out = Some((take(&stdout), take(&stderr)));
    match status {
        Err(e) => result(None, timed_out, Some(e.to_string()), out),
        Ok(_) if timed_out => result(
            None,
            true,
            Some(format!(
                "Precheck timed out after {}s.",
                timeout.as_secs().max(1)
            )),
            out,
        ),
        Ok(status) => result(status.code(), false, None, out),
    }
}

/// A detached worktree of a repository, removed when dropped.
pub struct Worktree {
    root: PathBuf,
    pub path: PathBuf,
}

fn git(root: &Path, args: &[&str]) -> Result<(), String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    match out.status.success() {
        true => Ok(()),
        false => Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

impl Worktree {
    /// `git worktree add --detach PATH BASE` in the repository at `root`;
    /// `base` defaults to its `HEAD`.
    pub fn add(root: &Path, path: &Path, base: Option<&str>) -> Result<Worktree, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        let target = path.to_string_lossy().into_owned();
        git(
            root,
            &[
                "worktree",
                "add",
                "--detach",
                "--quiet",
                &target,
                base.unwrap_or("HEAD"),
            ],
        )?;
        Ok(Worktree {
            root: root.to_path_buf(),
            path: path.to_path_buf(),
        })
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        let target = self.path.to_string_lossy().into_owned();
        if git(&self.root, &["worktree", "remove", "--force", &target]).is_err() {
            branchyard_support::cleanup_dir(&self.path);
            branchyard_support::best_effort(
                "prune the precheck worktrees",
                git(&self.root, &["worktree", "prune"]),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> PathBuf {
        std::env::temp_dir()
    }

    #[test]
    fn exit_codes_decide_and_output_is_kept() {
        let ok = run(
            "echo hello; echo oops >&2",
            Duration::from_secs(30),
            &dir(),
            &[],
        );
        assert!(ok.passed(), "{ok:?}");
        assert_eq!(ok.stdout, "hello\n");
        assert_eq!(ok.stderr, "oops\n");
        let skip = run("exit 3", Duration::from_secs(30), &dir(), &[]);
        assert!(!skip.passed());
        assert_eq!(skip.exit_code, Some(3));
        assert_eq!(failure(&skip), "Precheck exited with code 3.");
        let env = run(
            "test \"$BRANCHYARD_TRIGGER\" = nightly",
            Duration::from_secs(30),
            &dir(),
            &[("BRANCHYARD_TRIGGER".into(), "nightly".into())],
        );
        assert!(env.passed(), "{env:?}");
    }

    #[test]
    fn output_keeps_only_its_tail() {
        let long = run(
            "i=0; while [ $i -lt 1000 ]; do echo 0123456789; i=$((i+1)); done",
            Duration::from_secs(30),
            &dir(),
            &[],
        );
        assert!(long.stdout_truncated);
        assert_eq!(long.stdout.chars().count(), MAX_OUTPUT_CHARS);
        assert!(long.stdout.ends_with("0123456789\n"));
    }

    #[test]
    fn a_timeout_stops_the_whole_group() {
        // The shell's child would outlive a kill of the shell alone.
        let started = Instant::now();
        let slow = run(
            "sleep 30 & sleep 30; wait",
            Duration::from_secs(1),
            &dir(),
            &[],
        );
        assert!(slow.timed_out, "{slow:?}");
        assert!(!slow.passed());
        assert_eq!(slow.exit_code, None);
        assert!(failure(&slow).starts_with("Precheck timed out after"));
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the group was not stopped"
        );
    }

    #[test]
    fn normalization_follows_orca() {
        let p = |command: &str, timeout_seconds| Precheck {
            command: command.into(),
            timeout_seconds,
        };
        assert_eq!(normalize(&p("   ", 5)), None);
        assert_eq!(normalize(&p(" make check ", 0)), Some(p("make check", 1)));
        assert_eq!(
            normalize(&p("x", 10_000)),
            Some(p("x", MAX_TIMEOUT_SECONDS))
        );
        let missing = PrecheckResult {
            error: Some("No such file".into()),
            ..PrecheckResult::default()
        };
        assert_eq!(failure(&missing), "Precheck failed: No such file");
    }
}
