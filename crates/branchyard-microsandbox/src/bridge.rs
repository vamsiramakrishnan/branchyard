//! A guest exec's event stream presented as a synchronous [`Process`].
//!
//! The SDK reports an exec as a stream of events (started, stdout chunk,
//! stderr chunk, exited) and takes stdin writes and signals as messages.
//! [`BridgedProcess`] runs one thread per exec that copies the stream into
//! OS pipes, so callers read stdout and stderr with `std::io::Read` and get
//! end of file when the exec ends. It is independent of the SDK: the
//! provider supplies [`GuestEvents`] and [`GuestControl`] implementations,
//! and tests supply fakes.
//!
//! Guarantees: output is delivered in the order the guest reported it; the
//! exit status is recorded after all output before it has been written to
//! the pipes; stdout and stderr close when the exec ends, even if a
//! descendant still runs. Non-guarantees: memory is bounded only by the
//! SDK's unbounded event channel while a reader is slow.

use branchyard_support::{CondvarExt as _, LockExt as _};
use std::io::{self, PipeWriter, Read, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use branchyard_sandbox::{ExitStatus, Process};

/// One event from a guest exec, in the order the guest reported it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuestEvent {
    /// The process started with this guest PID. The guest agent makes it a
    /// session leader, so the PID is also its process-group ID.
    Started {
        pid: u32,
    },
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exited {
        code: i32,
    },
    /// The process could not be started.
    Failed {
        kind: io::ErrorKind,
        message: String,
    },
    /// A stdin write was not delivered. The process keeps running.
    StdinError(String),
}

/// A blocking source of one exec's events. `None` once the stream ends.
pub trait GuestEvents: Send + 'static {
    fn next(&mut self) -> Option<GuestEvent>;
}

/// Operations on one running exec.
pub trait GuestControl: Send + Sync + 'static {
    fn write_stdin(&self, data: &[u8]) -> io::Result<()>;
    fn close_stdin(&self) -> io::Result<()>;
    /// SIGKILL the exec's process group while the exec is registered.
    fn kill(&self) -> io::Result<()>;
    /// From inside the guest, name the live members of process group
    /// `pgid`, then SIGKILL the group ([`GROUP_TEARDOWN`]).
    fn teardown(&self, pgid: u32) -> io::Result<Vec<String>>;
}

/// A POSIX `sh` script, run as `sh -c SCRIPT sh PGID`, that prints the
/// command name of each live (non-zombie) process in group `PGID`, one per
/// line, from `/proc`, then sends the group SIGKILL. It needs only `sh` and
/// a Linux `/proc`, so it works in any image that has a shell.
pub const GROUP_TEARDOWN: &str = r#"g=$1
for f in /proc/[0-9]*/stat; do
  read -r s < "$f" 2>/dev/null || continue
  c=${s#*(}; c=${c%)*}
  set -- ${s##*) }
  [ "$3" = "$g" ] && [ "$1" != Z ] && printf '%s\n' "$c"
done
kill -s KILL -- "-$g" 2>/dev/null || kill -s KILL "-$g" 2>/dev/null
exit 0"#;

#[derive(Default)]
struct Exit {
    status: Mutex<Option<ExitStatus>>,
    changed: Condvar,
}

impl Exit {
    fn set(&self, status: ExitStatus) {
        let mut slot = self.status.lock_recovering("status");
        if slot.is_none() {
            *slot = Some(status);
            self.changed.notify_all();
        }
    }

    fn get(&self) -> Option<ExitStatus> {
        *self.status.lock_recovering("status")
    }

    fn wait(&self) -> ExitStatus {
        let mut slot = self.status.lock_recovering("status");
        loop {
            if let Some(status) = *slot {
                return status;
            }
            slot = self.changed.wait_recovering(slot, "changed");
        }
    }
}

/// A guest exec as a [`Process`]. See the module documentation.
pub struct BridgedProcess {
    pid: u32,
    control: Arc<dyn GuestControl>,
    exit: Arc<Exit>,
    stdin: Option<Box<dyn Write + Send>>,
    stdout: Option<Box<dyn Read + Send>>,
    stderr: Option<Box<dyn Read + Send>>,
    torn_down: bool,
}

impl BridgedProcess {
    /// Wait for the exec to start, then pump its events on a new thread.
    /// A [`GuestEvent::Failed`] before the start is an error of its kind.
    pub fn start(
        mut events: Box<dyn GuestEvents>,
        control: Arc<dyn GuestControl>,
    ) -> io::Result<BridgedProcess> {
        let mut early = Vec::new();
        let pid = loop {
            match events.next() {
                Some(GuestEvent::Started { pid }) => break pid,
                Some(GuestEvent::Failed { kind, message }) => {
                    return Err(io::Error::new(kind, message))
                }
                Some(event) => early.push(event),
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the exec ended before it started",
                    ))
                }
            }
        };
        let (stdout, stdout_writer) = io::pipe()?;
        let (stderr, stderr_writer) = io::pipe()?;
        let exit = Arc::new(Exit::default());
        let pump_exit = exit.clone();
        thread::Builder::new()
            .name(format!("guest-exec-{pid}"))
            .spawn(move || {
                let events = early
                    .into_iter()
                    .map(Some)
                    .chain(std::iter::from_fn(|| Some(events.next())));
                pump(events, stdout_writer, stderr_writer, &pump_exit);
            })?;
        Ok(BridgedProcess {
            pid,
            stdin: Some(Box::new(Stdin {
                control: control.clone(),
            })),
            control,
            exit,
            stdout: Some(Box::new(stdout) as Box<dyn Read + Send>),
            stderr: Some(Box::new(stderr) as Box<dyn Read + Send>),
            torn_down: false,
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }
}

fn pump(
    events: impl Iterator<Item = Option<GuestEvent>>,
    mut stdout: PipeWriter,
    mut stderr: PipeWriter,
    exit: &Exit,
) {
    let mut status = ExitStatus::default();
    for event in events {
        match event {
            // A reader that went away does not stop the exit from being
            // recorded.
            Some(GuestEvent::Stdout(bytes)) => drop(stdout.write_all(&bytes)),
            Some(GuestEvent::Stderr(bytes)) => drop(stderr.write_all(&bytes)),
            Some(GuestEvent::Exited { code }) => {
                status.code = Some(code);
                break;
            }
            // After a start, a failure or a lost stream leaves the status
            // unknown.
            Some(GuestEvent::Failed { .. }) | None => break,
            Some(GuestEvent::Started { .. } | GuestEvent::StdinError(_)) => {}
        }
    }
    drop((stdout, stderr));
    exit.set(status);
}

struct Stdin {
    control: Arc<dyn GuestControl>,
}

impl Write for Stdin {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            // An empty message means end of file to the guest agent.
            return Ok(0);
        }
        self.control.write_stdin(data)?;
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Stdin {
    #[allow(clippy::let_underscore_must_use)] // ratchet: branchyard-microsandbox
    fn drop(&mut self) {
        let _ = self.control.close_stdin();
    }
}

impl Process for BridgedProcess {
    fn id(&self) -> String {
        format!("guest pid {}", self.pid)
    }

    fn take_stdin(&mut self) -> Option<Box<dyn Write + Send>> {
        self.stdin.take()
    }

    fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
        self.stdout.take()
    }

    fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>> {
        self.stderr.take()
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        Ok(self.exit.get())
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        Ok(self.exit.wait())
    }

    /// SIGKILL to the whole process group: the guest agent signals the
    /// group, not only the launched process.
    fn kill(&mut self) -> io::Result<()> {
        match self.exit.get() {
            Some(_) => Ok(()),
            None => self.control.kill(),
        }
    }

    fn teardown(&mut self) -> Vec<String> {
        self.torn_down = true;
        self.control.teardown(self.pid).unwrap_or_default()
    }
}

impl Drop for BridgedProcess {
    fn drop(&mut self) {
        if self.exit.get().is_none() {
            branchyard_support::best_effort("kill control", self.control.kill());
        }
        if !self.torn_down {
            branchyard_support::best_effort(
                "tear down the microsandbox control process",
                self.control.teardown(self.pid),
            );
        }
    }
}

/// Parse [`GROUP_TEARDOWN`]'s output.
pub fn survivors(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

#[allow(clippy::unwrap_in_result)] // tests: a panic is the failure report
#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::*;

    /// Events fed by the test, blocking like the SDK's channel.
    struct Feed(mpsc::Receiver<GuestEvent>);

    impl GuestEvents for Feed {
        fn next(&mut self) -> Option<GuestEvent> {
            self.0.recv().ok()
        }
    }

    struct Script(VecDeque<GuestEvent>);

    impl GuestEvents for Script {
        fn next(&mut self) -> Option<GuestEvent> {
            self.0.pop_front()
        }
    }

    #[derive(Default)]
    struct Control {
        calls: Mutex<Vec<String>>,
    }

    impl Control {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl GuestControl for Control {
        fn write_stdin(&self, data: &[u8]) -> io::Result<()> {
            let text = String::from_utf8_lossy(data);
            self.calls.lock().unwrap().push(format!("stdin {text}"));
            Ok(())
        }
        fn close_stdin(&self) -> io::Result<()> {
            self.calls.lock().unwrap().push("close".into());
            Ok(())
        }
        fn kill(&self) -> io::Result<()> {
            self.calls.lock().unwrap().push("kill".into());
            Ok(())
        }
        fn teardown(&self, pgid: u32) -> io::Result<Vec<String>> {
            self.calls.lock().unwrap().push(format!("teardown {pgid}"));
            Ok(vec!["sleep".into()])
        }
    }

    fn read_all(mut pipe: Box<dyn Read + Send>) -> String {
        let mut text = String::new();
        pipe.read_to_string(&mut text).unwrap();
        text
    }

    #[test]
    fn output_and_exit_arrive_in_order() {
        let events = Script(VecDeque::from([
            GuestEvent::Stdout(b"early ".to_vec()),
            GuestEvent::Started { pid: 42 },
            GuestEvent::Stdout(b"hello\n".to_vec()),
            GuestEvent::Stderr(b"warn\n".to_vec()),
            GuestEvent::StdinError("closed".into()),
            GuestEvent::Stdout(b"bye\n".to_vec()),
            GuestEvent::Exited { code: 3 },
            GuestEvent::Stdout(b"after exit\n".to_vec()),
        ]));
        let control = Arc::new(Control::default());
        let mut process = BridgedProcess::start(Box::new(events), control.clone()).unwrap();
        assert_eq!(process.id(), "guest pid 42");
        assert_eq!(
            read_all(process.take_stdout().unwrap()),
            "early hello\nbye\n"
        );
        assert_eq!(read_all(process.take_stderr().unwrap()), "warn\n");
        let status = process.wait().unwrap();
        assert_eq!(status.code, Some(3));
        assert_eq!(process.try_wait().unwrap(), Some(status));
        process.kill().unwrap();
        assert_eq!(process.teardown(), ["sleep"]);
        drop(process);
        assert_eq!(control.calls(), ["teardown 42", "close"]);
    }

    #[test]
    fn a_failed_start_is_an_error_of_its_kind() {
        let events = Script(VecDeque::from([GuestEvent::Failed {
            kind: io::ErrorKind::NotFound,
            message: "claude: not found".into(),
        }]));
        let error = BridgedProcess::start(Box::new(events), Arc::new(Control::default()))
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        let ended = Script(VecDeque::new());
        let error = BridgedProcess::start(Box::new(ended), Arc::new(Control::default()))
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_lost_stream_leaves_the_status_unknown_and_closes_output() {
        let events = Script(VecDeque::from([
            GuestEvent::Started { pid: 7 },
            GuestEvent::Stdout(b"partial".to_vec()),
        ]));
        let mut process =
            BridgedProcess::start(Box::new(events), Arc::new(Control::default())).unwrap();
        assert_eq!(read_all(process.take_stdout().unwrap()), "partial");
        assert_eq!(process.wait().unwrap(), ExitStatus::default());
    }

    #[test]
    fn stdin_is_forwarded_and_dropping_a_running_exec_kills_it() {
        let (feed, events) = mpsc::channel();
        feed.send(GuestEvent::Started { pid: 9 }).unwrap();
        let control = Arc::new(Control::default());
        let mut process = BridgedProcess::start(Box::new(Feed(events)), control.clone()).unwrap();
        let mut stdin = process.take_stdin().unwrap();
        assert!(process.take_stdin().is_none());
        stdin.write_all(b"line\n").unwrap();
        assert_eq!(stdin.write(b"").unwrap(), 0);
        drop(stdin);
        assert_eq!(process.try_wait().unwrap(), None);
        process.kill().unwrap();
        drop(process);
        assert_eq!(
            control.calls(),
            ["stdin line\n", "close", "kill", "kill", "teardown 9"]
        );
        drop(feed);
    }

    #[test]
    fn a_slow_reader_does_not_lose_output() {
        let (feed, events) = mpsc::channel();
        feed.send(GuestEvent::Started { pid: 1 }).unwrap();
        let mut process =
            BridgedProcess::start(Box::new(Feed(events)), Arc::new(Control::default())).unwrap();
        let chunk = vec![b'x'; 64 * 1024];
        for _ in 0..16 {
            feed.send(GuestEvent::Stdout(chunk.clone())).unwrap();
        }
        feed.send(GuestEvent::Exited { code: 0 }).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        // The pipe is full: the exit is not recorded before the output.
        assert_eq!(process.try_wait().unwrap(), None);
        assert_eq!(
            read_all(process.take_stdout().unwrap()).len(),
            16 * 64 * 1024
        );
        assert!(process.wait().unwrap().success());
    }

    fn alive(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map(|stat| {
                let state = stat.rsplit_once(") ").map(|(_, rest)| rest);
                !state.is_some_and(|s| s.starts_with('Z'))
            })
            .unwrap_or(false)
    }

    /// The guest teardown script, run against a local process group.
    #[test]
    fn the_group_teardown_script_names_and_kills_a_group() {
        if !std::path::Path::new("/proc/self/stat").exists() {
            return;
        }
        let mut leader = Command::new("sh")
            .args(["-c", "sleep 300 & echo $!; wait"])
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut line = String::new();
        io::BufRead::read_line(
            &mut io::BufReader::new(leader.stdout.take().unwrap()),
            &mut line,
        )
        .unwrap();
        let child: u32 = line.trim().parse().unwrap();
        // `$!` is known as soon as the shell forks, before the child has
        // exec'd `sleep`; until it has, it is still named `sh`.
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::fs::read_to_string(format!("/proc/{child}/comm"))
            .map(|comm| comm.trim() != "sleep")
            .unwrap_or(true)
        {
            assert!(Instant::now() < deadline, "{child} never became sleep");
            std::thread::sleep(Duration::from_millis(5));
        }
        let output = Command::new("sh")
            .args(["-c", GROUP_TEARDOWN, "sh", &leader.id().to_string()])
            .output()
            .unwrap();
        assert!(output.status.success());
        let mut names = survivors(&String::from_utf8_lossy(&output.stdout));
        names.sort();
        assert_eq!(names, ["sh", "sleep"]);
        let started = Instant::now();
        leader.wait().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the leader outlived the teardown"
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while alive(child) {
            assert!(Instant::now() < deadline, "{child} survived the teardown");
            std::thread::sleep(Duration::from_millis(20));
        }
        let empty = Command::new("sh")
            .args(["-c", GROUP_TEARDOWN, "sh", &leader.id().to_string()])
            .output()
            .unwrap();
        assert!(empty.status.success());
        assert!(survivors(&String::from_utf8_lossy(&empty.stdout)).is_empty());
    }
}
