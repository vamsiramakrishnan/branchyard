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

/// The variable every local harness is started with, naming its turn's
/// `start` step, so recovery can find the harness's processes even when
/// the engine stopped before recording the harness's pid.
pub(crate) const ENV_SPAWN: &str = "BRANCHYARD_SPAWN";

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

/// SIGKILL `pid` if it is still the process that started at `start`;
/// whether it was signalled. A reused pid is never signalled.
pub(crate) fn kill(pid: u32, start: &str) -> bool {
    if pid <= 1 || pid == std::process::id() || !alive(pid, start) {
        return false;
    }
    signal(pid);
    true
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
            signal_group(pgid);
            members
        }
        Some(_) => Vec::new(),
        None => {
            let members = members(pgid, start);
            for pid in &members {
                signal(*pid);
            }
            members
        }
    }
}

/// SIGKILL every process on this host whose environment has
/// [`ENV_SPAWN`] set to `marker`, except this one, and return the pids
/// signalled. Linux only: elsewhere nothing is found. A process sees only
/// the environment it was started with, so this finds the harness and
/// whatever it started without clearing that variable, in its process
/// group or out of it.
pub(crate) fn kill_marked(marker: &str) -> Vec<u32> {
    if marker.is_empty() {
        return Vec::new();
    }
    let mut killed = Vec::new();
    // Again until none is left, for a process that forked meanwhile.
    for _ in 0..5 {
        let found = marked(marker);
        if found.is_empty() {
            break;
        }
        for pid in &found {
            signal(*pid);
        }
        killed.extend(found);
    }
    killed.sort_unstable();
    killed.dedup();
    killed
}

#[cfg(target_os = "linux")]
fn marked(marker: &str) -> Vec<u32> {
    let wanted = format!("{ENV_SPAWN}={marker}");
    let own = std::process::id();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut pids: Vec<u32> = entries
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| *pid != own)
        .filter(|pid| {
            stat(*pid).is_some_and(|fields| fields.first().map(String::as_str) != Some("Z"))
                && std::fs::read(format!("/proc/{pid}/environ")).is_ok_and(|environ| {
                    environ
                        .split(|byte| *byte == 0)
                        .any(|entry| entry == wanted.as_bytes())
                })
        })
        .collect();
    pids.sort_unstable();
    pids
}

#[cfg(not(target_os = "linux"))]
fn marked(_marker: &str) -> Vec<u32> {
    Vec::new()
}

/// SIGKILL `pid`, through kill(2) rather than a `kill` process. Best
/// effort: a process already gone is not an error.
fn signal(pid: u32) {
    #[cfg(unix)]
    if let Some(pid) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    {
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
    }
}

/// SIGKILL every process in group `pgid`, through killpg(2).
fn signal_group(pgid: u32) {
    #[cfg(unix)]
    if let Some(pgid) = i32::try_from(pgid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    {
        let _ = rustix::process::kill_process_group(pgid, rustix::process::Signal::KILL);
    }
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

    #[cfg(target_os = "linux")]
    #[test]
    fn processes_carrying_a_marker_are_killed_whatever_their_group() {
        use std::os::unix::process::CommandExt;
        let marker = format!("proc-test-{}", std::process::id());
        let spawn = |marker: &str| {
            Command::new("sleep")
                .arg("30")
                .env(ENV_SPAWN, marker)
                .process_group(0)
                .spawn()
                .unwrap()
        };
        let (mut a, mut b) = (spawn(&marker), spawn(&marker));
        let mut other = spawn(&format!("{marker}-other"));
        // `spawn` returns once the child is inside `execve`, before the
        // kernel publishes its new environment; wait until it has.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        for child in [&a, &b, &other] {
            let path = format!("/proc/{}/environ", child.id());
            while !std::fs::read(&path).is_ok_and(|environ| {
                environ
                    .split(|byte| *byte == 0)
                    .any(|entry| entry.starts_with(ENV_SPAWN.as_bytes()))
            }) {
                assert!(std::time::Instant::now() < deadline, "{path} stayed empty");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        let mut expected = vec![a.id(), b.id()];
        expected.sort_unstable();
        assert_eq!(kill_marked(&marker), expected);
        assert!(!a.wait().unwrap().success());
        assert!(!b.wait().unwrap().success());
        assert!(
            other.try_wait().unwrap().is_none(),
            "another marker is kept"
        );
        assert!(kill_marked("").is_empty());
        other.kill().unwrap();
        other.wait().unwrap();
    }
}
