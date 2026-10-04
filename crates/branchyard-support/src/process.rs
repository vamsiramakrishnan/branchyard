//! Signals to processes and process groups, where "already gone" is success.

use crate::best_effort::report;

/// SIGKILL process group `pgid`, through killpg(2). A group already gone is
/// success; any other failure is reported. No-op off Unix.
pub fn kill_group(pgid: u32) {
    #[cfg(unix)]
    signal_group(pgid, rustix::process::Signal::KILL, "kill");
    #[cfg(not(unix))]
    let _ = pgid;
}

/// SIGTERM process group `pgid`, asking it to stop. See [`kill_group`].
pub fn terminate_group(pgid: u32) {
    #[cfg(unix)]
    signal_group(pgid, rustix::process::Signal::TERM, "terminate");
    #[cfg(not(unix))]
    let _ = pgid;
}

/// SIGKILL process `pid`, through kill(2). A process already gone is
/// success; any other failure is reported. No-op off Unix.
pub fn kill_process(pid: u32) {
    #[cfg(unix)]
    match i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    {
        Some(target) => {
            if let Err(e) = rustix::process::kill_process(target, rustix::process::Signal::KILL) {
                if e != rustix::io::Errno::SRCH {
                    report(&format!("kill process {pid}"), &e);
                }
            }
        }
        None => report(&format!("kill process {pid}"), &"not a valid process id"),
    }
    #[cfg(not(unix))]
    let _ = pid;
}

#[cfg(unix)]
fn signal_group(pgid: u32, signal: rustix::process::Signal, what: &str) {
    match i32::try_from(pgid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    {
        Some(target) => {
            if let Err(e) = rustix::process::kill_process_group(target, signal) {
                if e != rustix::io::Errno::SRCH {
                    report(&format!("{what} process group {pgid}"), &e);
                }
            }
        }
        None => report(
            &format!("{what} process group {pgid}"),
            &"not a valid process group id",
        ),
    }
}
