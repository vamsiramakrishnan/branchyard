//! Process identity for recovery: a process is named by its pid and its
//! start time, so a recycled pid is never mistaken for the process that was
//! recorded, and by the host it runs on, including the boot, so a pid from
//! before a reboot is never signalled.
//!
//! On Linux the start time is `/proc/<pid>/stat`'s `starttime` (clock ticks
//! since boot) and the boot is `/proc/sys/kernel/random/boot_id`. Elsewhere
//! it is `ps -o lstart=` (one-second resolution) and `sysctl -n
//! kern.boottime`.

use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// This host and its current boot, as `hostname/boot`.
pub(crate) fn host() -> &'static str {
    static HOST: OnceLock<String> = OnceLock::new();
    HOST.get_or_init(|| {
        let name = std::fs::read_to_string("/proc/sys/kernel/hostname")
            .ok()
            .or_else(|| output("uname", &["-n"]))
            .unwrap_or_default();
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .ok()
            .or_else(|| output("sysctl", &["-n", "kern.boottime"]))
            .unwrap_or_default();
        format!("{}/{}", name.trim(), boot.trim())
    })
}

/// This process's start time.
pub(crate) fn own_start() -> &'static str {
    static START: OnceLock<String> = OnceLock::new();
    START.get_or_init(|| start_time(std::process::id()).unwrap_or_default())
}

fn output(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && !text.is_empty()).then_some(text)
}

/// The fields of `/proc/<pid>/stat` after the command name: state is
/// index 0, the process group 2, the start time 19.
#[cfg(target_os = "linux")]
fn stat(pid: u32) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &text[text.rfind(')')? + 1..];
    Some(rest.split_whitespace().map(str::to_owned).collect())
}

/// When `pid` started, or `None` if no such process exists (a zombie
/// counts as gone).
pub(crate) fn start_time(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let fields = stat(pid)?;
        if fields.first().map(String::as_str) == Some("Z") {
            return None;
        }
        fields.get(19).cloned()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let state = output("ps", &["-o", "stat=", "-p", &pid.to_string()])?;
        if state.starts_with('Z') {
            return None;
        }
        output("ps", &["-o", "lstart=", "-p", &pid.to_string()])
    }
}

/// Whether the process recorded as `pid` started at `start` is still
/// running.
pub(crate) fn alive(pid: u32, start: &str) -> bool {
    !start.is_empty() && start_time(pid).as_deref() == Some(start)
}

/// SIGKILL what is left of the process group led by `pgid`, which started
/// at `start`, and return the pids signalled.
///
/// The group is signalled as a whole only while its leader is the recorded
/// process; a live process with the leader's pid and another start time
/// means the pid was reused, and nothing is signalled. Once the leader has
/// exited, on Linux the remaining members that started no earlier than it
/// are signalled one by one; elsewhere nothing is signalled.
pub(crate) fn kill_group(pgid: u32, start: &str) -> Vec<u32> {
    if pgid <= 1 || start.is_empty() {
        return Vec::new();
    }
    match start_time(pgid) {
        Some(current) if current == start => {
            let members = members(pgid, start);
            signal(&format!("-{pgid}"));
            members
        }
        Some(_) => Vec::new(),
        None => {
            let members = members(pgid, start);
            for pid in &members {
                signal(&pid.to_string());
            }
            members
        }
    }
}

fn signal(target: &str) {
    let _ = Command::new("kill")
        .args(["-KILL", "--", target])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Live members of process group `pgid` that started no earlier than
/// `start`.
#[cfg(target_os = "linux")]
fn members(pgid: u32, start: &str) -> Vec<u32> {
    let Ok(start) = start.parse::<u64>() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut pids: Vec<u32> = entries
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            stat(*pid).is_some_and(|fields| {
                fields.first().map(String::as_str) != Some("Z")
                    && fields.get(2).and_then(|g| g.parse::<u32>().ok()) == Some(pgid)
                    && fields
                        .get(19)
                        .and_then(|s| s.parse::<u64>().ok())
                        .is_some_and(|s| s >= start)
            })
        })
        .collect();
    pids.sort_unstable();
    pids
}

#[cfg(not(target_os = "linux"))]
fn members(pgid: u32, start: &str) -> Vec<u32> {
    match alive(pgid, start) {
        true => vec![pgid],
        false => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_is_alive_and_a_wrong_start_is_not() {
        let pid = std::process::id();
        let start = own_start();
        assert!(!start.is_empty());
        assert!(alive(pid, start));
        assert!(!alive(pid, "0"));
        assert!(!host().is_empty());
    }

    #[test]
    fn a_group_is_killed_only_while_its_start_matches() {
        use std::os::unix::process::CommandExt;
        let mut child = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id();
        let start = start_time(pid).unwrap();
        assert!(kill_group(pid, "1").is_empty());
        assert!(alive(pid, &start), "a wrong start time signals nothing");
        assert_eq!(kill_group(pid, &start), [pid]);
        let status = child.wait().unwrap();
        assert!(!status.success());
        assert!(!alive(pid, &start));
    }
}
