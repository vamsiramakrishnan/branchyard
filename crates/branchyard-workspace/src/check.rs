//! Running a trusted check command with a timeout.

use std::collections::VecDeque;
use std::io::{self, Read};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::git::SCRUBBED_ENV;

/// Bytes of combined stdout/stderr kept from a check.
pub const OUTPUT_TAIL_BYTES: usize = 4096;

/// A command run in the integration worktree, without a shell.
///
/// `argv[0]` is looked up on `PATH`. The working directory is the merged
/// tree; stdin is empty; stdout and stderr are combined. On Unix the command
/// runs in its own process group, which is killed when the command exits or
/// times out so no descendants outlive the integration worktree. Processes
/// that leave that group (for example with `setsid`) are not tracked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub argv: Vec<String>,
    pub timeout: Duration,
}

pub(crate) enum CheckOutcome {
    Exited(ExitStatus),
    TimedOut,
}

pub(crate) fn run(check: &Check, dir: &Path) -> io::Result<(CheckOutcome, String)> {
    let Some((program, args)) = check.argv.split_first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "check argv is empty",
        ));
    };
    let (reader, writer) = io::pipe()?;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(writer.try_clone()?)
        .stderr(writer);
    for var in SCRUBBED_ENV {
        cmd.env_remove(var);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn()?;
    // The Command holds our copies of the pipe's write end; closing them lets
    // the reader see EOF once the check's processes exit.
    drop(cmd);

    let tail = Arc::new(Mutex::new(VecDeque::with_capacity(OUTPUT_TAIL_BYTES)));
    let (done_tx, done_rx) = mpsc::channel();
    {
        let tail = Arc::clone(&tail);
        let mut reader = reader;
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                let mut tail = tail.lock().unwrap_or_else(|e| e.into_inner());
                tail.extend(&buf[..n]);
                let excess = tail.len().saturating_sub(OUTPUT_TAIL_BYTES);
                tail.drain(..excess);
            }
            let _ = done_tx.send(());
        });
    }

    let deadline = Instant::now() + check.timeout;
    let mut pause = Duration::from_millis(5);
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) => break CheckOutcome::Exited(status),
            Ok(None) => {}
            Err(e) => {
                kill(&mut child);
                return Err(e);
            }
        }
        let now = Instant::now();
        if now >= deadline {
            kill(&mut child);
            break CheckOutcome::TimedOut;
        }
        thread::sleep(pause.min(deadline - now));
        pause = (pause * 2).min(Duration::from_millis(50));
    };
    // Reap stragglers still in the group. The group ID cannot be reused while
    // any member is alive, so this only reaches the check's own processes
    // unless the group is already empty and its ID recycled within this
    // window.
    kill_group(child.id());
    // A process that escaped the group may hold the pipe open; do not wait on it.
    let _ = done_rx.recv_timeout(Duration::from_secs(1));
    let bytes: Vec<u8> = tail
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .copied()
        .collect();
    Ok((outcome, String::from_utf8_lossy(&bytes).into_owned()))
}

/// Kills the check's process group while its leader is still unreaped, then
/// the leader itself, and reaps it.
fn kill(child: &mut Child) {
    kill_group(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
fn kill_group(pgid: u32) {
    // std has no killpg; rustix calls killpg(2) directly.
    if let Some(pgid) = i32::try_from(pgid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    {
        let _ = rustix::process::kill_process_group(pgid, rustix::process::Signal::KILL);
    }
}

#[cfg(not(unix))]
fn kill_group(_pgid: u32) {}
