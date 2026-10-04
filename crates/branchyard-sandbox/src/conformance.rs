//! Conformance checks every [`SandboxProvider`] must pass.
//!
//! Like `assert_eq!`, each check panics with a message naming what failed,
//! so it reads naturally inside a `#[test]`. Each creates its own sandbox,
//! named `<prefix>-<check>`, and destroys it afterwards, even on failure.
//!
//! The checks run `sh` inside the sandbox and read `/proc`, so the sandbox
//! needs a POSIX shell with `sleep`, `cat` and `printf`, and a Linux
//! `/proc`. They do not check isolation: a local provider passes them.
//!
//! [`egress_confinement`] runs `python3` in the sandbox when the provider
//! declares [`crate::Capabilities::egress`]; a provider that does not must
//! refuse [`SandboxProvider::exec_confined`] instead.
//!
//! A provider that cannot show host directories to a sandbox at all, such
//! as one whose sandboxes run on another machine, is checked with
//! [`Setup::without_mounts`]: its sandboxes are created without mounts and
//! work in a directory that already exists in the sandbox, and the two
//! mount checks instead require that a spec with a mount is refused, never
//! created with the mount silently missing.
//!
//! ```no_run
//! use branchyard_sandbox::conformance::{self, Setup};
//! # fn provider() -> Box<dyn branchyard_sandbox::SandboxProvider> { unimplemented!() }
//! let provider = provider();
//! let setup = Setup::new("by-conf", "/tmp/workspace", "/workspace");
//! conformance::run_all(provider.as_ref(), &setup);
//! ```

#![allow(
    clippy::expect_used,
    clippy::let_underscore_must_use,
    clippy::panic,
    clippy::unwrap_used
)] // ratchet: branchyard-sandbox
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use crate::provider::{ExecSpec, Mount, Process, ProviderError, SandboxProvider, SandboxSpec};
use crate::{Resources, SandboxState};

/// How the checks create their sandboxes.
#[derive(Clone, Debug)]
pub struct Setup {
    /// Prefix for sandbox names.
    pub prefix: String,
    pub image: Option<String>,
    pub resources: Resources,
    /// An existing host directory, mounted writable into every sandbox.
    /// Checks write files into it.
    pub workspace: PathBuf,
    /// Where the workspace appears in the sandbox; with
    /// [`Setup::mounts`] off, a directory that exists in every sandbox.
    pub guest_workspace: PathBuf,
    /// Whether the provider mounts host directories. See
    /// [`Setup::without_mounts`].
    pub mounts: bool,
    /// Variables every exec sets, such as `PATH` for a provider whose base
    /// environment is empty.
    pub env: BTreeMap<OsString, OsString>,
    /// How long any one step may take.
    pub timeout: Duration,
}

impl Setup {
    pub fn new(
        prefix: impl Into<String>,
        workspace: impl Into<PathBuf>,
        guest_workspace: impl Into<PathBuf>,
    ) -> Setup {
        Setup {
            prefix: prefix.into(),
            image: None,
            resources: Resources::default(),
            workspace: workspace.into(),
            guest_workspace: guest_workspace.into(),
            mounts: true,
            env: BTreeMap::new(),
            timeout: Duration::from_secs(30),
        }
    }

    /// For a provider that cannot mount host directories: sandboxes get no
    /// mount, processes run in `guest_workspace`, which must already exist
    /// in every sandbox, and a spec with a mount must be refused.
    pub fn without_mounts(prefix: impl Into<String>, guest_workspace: impl Into<PathBuf>) -> Setup {
        let guest_workspace = guest_workspace.into();
        Setup {
            mounts: false,
            ..Setup::new(prefix, guest_workspace.clone(), guest_workspace)
        }
    }

    fn spec(&self, check: &str) -> SandboxSpec {
        let mut spec = self.mounted(check);
        if !self.mounts {
            spec.mounts.clear();
        }
        spec
    }

    /// The spec with the workspace mounted, whether or not the provider can.
    fn mounted(&self, check: &str) -> SandboxSpec {
        SandboxSpec {
            name: format!("{}-{check}", self.prefix),
            image: self.image.clone(),
            resources: self.resources.clone(),
            mounts: vec![Mount::writable(
                self.workspace.clone(),
                self.guest_workspace.clone(),
            )],
            persist: false,
        }
    }

    fn exec(&self, script: &str) -> ExecSpec {
        let mut env = self.env.clone();
        env.insert("BY_CONFORMANCE".into(), "set by the exec".into());
        ExecSpec {
            argv: vec!["sh".into(), "-c".into(), script.into()],
            cwd: self.guest_workspace.clone(),
            env,
        }
    }
}

/// Every check, in order.
pub fn run_all(provider: &dyn SandboxProvider, setup: &Setup) {
    lifecycle(provider, setup);
    exit_status(provider, setup);
    missing_program(provider, setup);
    stdio_round_trip(provider, setup);
    env_and_cwd(provider, setup);
    workspace_mount(provider, setup);
    read_only_mount(provider, setup);
    kill_reaches_descendants(provider, setup);
    teardown_names_survivors(provider, setup);
    drop_tears_down(provider, setup);
    stop_ends_processes(provider, setup);
    egress_confinement(provider, setup);
}

/// Destroys its sandbox when dropped.
struct Sandbox<'a> {
    provider: &'a dyn SandboxProvider,
    name: String,
    setup: &'a Setup,
}

impl<'a> Sandbox<'a> {
    fn ensure(provider: &'a dyn SandboxProvider, setup: &'a Setup, check: &str) -> Self {
        Sandbox::ensure_spec(provider, setup, setup.spec(check))
    }

    fn ensure_spec(provider: &'a dyn SandboxProvider, setup: &'a Setup, spec: SandboxSpec) -> Self {
        branchyard_support::best_effort("provider.destroy", provider.destroy(&spec.name));
        let info = provider
            .ensure(&spec)
            .unwrap_or_else(|e| panic!("ensure {}: {e}", spec.name));
        assert_eq!(info.name, spec.name, "ensure names the sandbox it made");
        Sandbox {
            provider,
            name: spec.name,
            setup,
        }
    }

    fn spawn(&self, script: &str) -> Box<dyn Process> {
        self.provider
            .exec(&self.name, &self.setup.exec(script))
            .unwrap_or_else(|e| panic!("exec {script:?}: {e}"))
    }

    /// Run `script` to completion, returning its code, stdout and stderr.
    fn run(&self, script: &str) -> (Option<i32>, String, String) {
        let mut process = self.spawn(script);
        drop(process.take_stdin());
        let out = drain(process.take_stdout().expect("stdout is piped"));
        let err = drain(process.take_stderr().expect("stderr is piped"));
        let status = process.wait().expect("wait");
        let (out, err) = (out.join().unwrap(), err.join().unwrap());
        (status.code, out, err)
    }

    /// Whether `pid` is a live, non-zombie process in the sandbox.
    fn alive(&self, pid: u32) -> bool {
        let script = format!(
            "if [ -r /proc/{pid}/stat ]; then read -r s < /proc/{pid}/stat; \
             s=${{s##*) }}; case $s in Z*) echo gone;; *) echo alive;; esac; \
             else echo gone; fi"
        );
        let (_, out, err) = self.run(&script);
        match out.trim() {
            "alive" => true,
            "gone" => false,
            other => panic!("liveness probe printed {other:?}, stderr {err:?}"),
        }
    }

    /// Wait until `pid` runs the program `name`, by its `/proc/<pid>/comm`.
    /// A shell knows a background child's PID (`$!`) as soon as it forks,
    /// before the child has exec'd its program; until then, which under
    /// load can outlast the shell itself, the child is still named `sh`.
    fn await_exec(&self, pid: u32, name: &str) {
        let deadline = Instant::now() + self.setup.timeout;
        loop {
            let (_, comm, _) = self.run(&format!("cat /proc/{pid}/comm 2>/dev/null"));
            if comm.trim() == name {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "pid {pid} never became {name}: comm {comm:?}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait until `pid` is gone, or panic naming `what`.
    fn assert_gone(&self, pid: u32, what: &str) {
        let deadline = Instant::now() + self.setup.timeout;
        while self.alive(pid) {
            assert!(Instant::now() < deadline, "{what}: pid {pid} survived");
            thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Sandbox<'_> {
    fn drop(&mut self) {
        let destroyed = self.provider.destroy(&self.name);
        if !thread::panicking() {
            destroyed.unwrap_or_else(|e| panic!("destroy {}: {e}", self.name));
        }
    }
}

fn drain(mut pipe: Box<dyn Read + Send>) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut text = String::new();
        let _ = pipe.read_to_string(&mut text);
        text
    })
}

/// The first line a process prints, parsed as a PID.
fn first_pid(process: &mut dyn Process) -> (u32, BufReader<Box<dyn Read + Send>>) {
    let mut reader = BufReader::new(process.take_stdout().expect("stdout is piped"));
    let mut line = String::new();
    reader.read_line(&mut line).expect("read the pid line");
    let pid = line
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("expected a pid, got {line:?}"));
    (pid, reader)
}

/// `ensure` is idempotent, `inspect` follows the lifecycle and `destroy`
/// forgets the sandbox.
pub fn lifecycle(provider: &dyn SandboxProvider, setup: &Setup) {
    let sandbox = Sandbox::ensure(provider, setup, "lifecycle");
    let again = provider
        .ensure(&setup.spec("lifecycle"))
        .expect("a second ensure");
    assert_eq!(again.name, sandbox.name);
    let info = provider.inspect(&sandbox.name).expect("inspect");
    assert_eq!(
        info.map(|i| i.state),
        Some(SandboxState::Running),
        "an ensured sandbox is running"
    );
    provider.stop(&sandbox.name).expect("stop");
    let stopped = provider.inspect(&sandbox.name).expect("inspect after stop");
    assert!(
        stopped
            .as_ref()
            .is_none_or(|i| i.state != SandboxState::Running),
        "a stopped sandbox is not running: {stopped:?}"
    );
    provider.destroy(&sandbox.name).expect("destroy");
    assert_eq!(provider.inspect(&sandbox.name).expect("inspect"), None);
    provider
        .destroy(&sandbox.name)
        .expect("destroying a missing sandbox is not an error");
}

/// The exit code reaches `wait` and `try_wait`.
pub fn exit_status(provider: &dyn SandboxProvider, setup: &Setup) {
    let sandbox = Sandbox::ensure(provider, setup, "exit-status");
    let mut process = sandbox.spawn("exit 7");
    let status = process.wait().expect("wait");
    assert_eq!(status.code, Some(7), "{status}");
    assert!(!status.success());
    assert_eq!(process.try_wait().expect("try_wait"), Some(status));
    let (code, _, _) = sandbox.run("true");
    assert_eq!(code, Some(0));
}

/// A program that cannot start fails `exec` with an I/O error.
pub fn missing_program(provider: &dyn SandboxProvider, setup: &Setup) {
    let sandbox = Sandbox::ensure(provider, setup, "missing-program");
    let mut spec = setup.exec("");
    spec.argv = vec!["/nonexistent/by-conformance-program".into()];
    match provider.exec(&sandbox.name, &spec) {
        Err(ProviderError::Io(_)) => {}
        Err(other) => panic!("expected an I/O error, got {other}"),
        Ok(mut process) => {
            let status = process.wait();
            panic!("a missing program started and ended with {status:?}")
        }
    }
    spec.argv.clear();
    assert!(provider.exec(&sandbox.name, &spec).is_err(), "empty argv");
}

/// Stdin reaches the process; stdout and stderr come back separately.
pub fn stdio_round_trip(provider: &dyn SandboxProvider, setup: &Setup) {
    let sandbox = Sandbox::ensure(provider, setup, "stdio");
    let mut process =
        sandbox.spawn(r#"while read -r line; do echo "out:$line"; echo "err:$line" >&2; done"#);
    let mut stdin = process.take_stdin().expect("stdin is piped");
    let out = drain(process.take_stdout().expect("stdout is piped"));
    let err = drain(process.take_stderr().expect("stderr is piped"));
    assert!(process.take_stdout().is_none(), "stdout is taken once");
    stdin.write_all(b"alpha\n").unwrap();
    stdin.flush().unwrap();
    stdin.write_all(b"beta\n").unwrap();
    drop(stdin);
    let status = process.wait().expect("wait");
    assert!(status.success(), "{status}");
    assert_eq!(out.join().unwrap(), "out:alpha\nout:beta\n");
    assert_eq!(err.join().unwrap(), "err:alpha\nerr:beta\n");
}

/// The process runs in the requested directory with the requested variables.
pub fn env_and_cwd(provider: &dyn SandboxProvider, setup: &Setup) {
    let sandbox = Sandbox::ensure(provider, setup, "env-cwd");
    let (code, out, err) =
        sandbox.run(r#"printf '%s\n%s\n%s\n' "$BY_CONFORMANCE" "$(pwd -P)" "${BY_UNSET-unset}""#);
    assert_eq!(code, Some(0), "{err}");
    let expected = format!(
        "set by the exec\n{}\nunset\n",
        setup.guest_workspace.display()
    );
    assert_eq!(out, expected);
}

/// A provider without mounts refuses a spec that has one, and creates
/// nothing.
fn mount_refused(provider: &dyn SandboxProvider, mut spec: SandboxSpec) {
    spec.name.push_str("-refused");
    branchyard_support::best_effort("provider.destroy", provider.destroy(&spec.name));
    match provider.ensure(&spec) {
        Err(ProviderError::Invalid(_) | ProviderError::Unsupported(_)) => {}
        Err(other) => panic!("ensure with a mount it cannot honor: {other}"),
        Ok(_) => {
            branchyard_support::best_effort("provider.destroy", provider.destroy(&spec.name));
            panic!("a provider without mounts created a sandbox with a mount")
        }
    }
    assert_eq!(
        provider.inspect(&spec.name).expect("inspect"),
        None,
        "a refused spec created a sandbox"
    );
}

/// Files cross the workspace mount in both directions. Without mounts, a
/// mount is refused.
pub fn workspace_mount(provider: &dyn SandboxProvider, setup: &Setup) {
    if !setup.mounts {
        return mount_refused(provider, setup.mounted("mount"));
    }
    let sandbox = Sandbox::ensure(provider, setup, "mount");
    std::fs::write(setup.workspace.join("from-host.txt"), "host\n").unwrap();
    let (code, out, err) = sandbox.run("cat from-host.txt && printf guest > from-guest.txt");
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out, "host\n");
    let written = std::fs::read_to_string(setup.workspace.join("from-guest.txt"))
        .expect("the guest's write reaches the host");
    assert_eq!(written, "guest");
    branchyard_support::cleanup_file(setup.workspace.join("from-host.txt"));
    branchyard_support::cleanup_file(setup.workspace.join("from-guest.txt"));
}

/// A read-only mount is either refused or enforced; never silently writable.
pub fn read_only_mount(provider: &dyn SandboxProvider, setup: &Setup) {
    let mut spec = setup.mounted("read-only");
    spec.mounts[0].writable = false;
    if !setup.mounts {
        return mount_refused(provider, spec);
    }
    branchyard_support::best_effort("provider.destroy", provider.destroy(&spec.name));
    match provider.ensure(&spec) {
        Err(ProviderError::Invalid(_) | ProviderError::Unsupported(_)) => {
            branchyard_support::best_effort("provider.destroy", provider.destroy(&spec.name));
        }
        Err(other) => panic!("ensure with a read-only mount: {other}"),
        Ok(_) => {
            let sandbox = Sandbox {
                provider,
                name: spec.name.clone(),
                setup,
            };
            let (code, _, _) = sandbox.run("printf x > read-only.txt");
            assert_ne!(code, Some(0), "a read-only mount accepted a write");
            assert!(!setup.workspace.join("read-only.txt").exists());
        }
    }
}

/// `kill` and `teardown` end the process and its background children.
pub fn kill_reaches_descendants(provider: &dyn SandboxProvider, setup: &Setup) {
    let sandbox = Sandbox::ensure(provider, setup, "kill");
    let mut process = sandbox.spawn("sleep 300 & echo $!; wait");
    let (child, _reader) = first_pid(process.as_mut());
    assert!(sandbox.alive(child));
    process.kill().expect("kill");
    let status = process.wait().expect("wait after kill");
    assert!(!status.success(), "a killed process succeeded: {status}");
    process.teardown();
    sandbox.assert_gone(child, "kill then teardown");
}

/// After the launched process exits, `teardown` names and kills what it
/// left running.
pub fn teardown_names_survivors(provider: &dyn SandboxProvider, setup: &Setup) {
    let sandbox = Sandbox::ensure(provider, setup, "teardown");
    let mut process = sandbox.spawn("sleep 300 & echo $!");
    let (child, _reader) = first_pid(process.as_mut());
    // Teardown names what it finds; a child it finds before the exec is
    // rightly named `sh`, so wait for the exec this check asks it to name.
    sandbox.await_exec(child, "sleep");
    let status = process.wait().expect("wait");
    assert!(status.success(), "{status}");
    assert!(
        sandbox.alive(child),
        "the background child outlived its parent"
    );
    let survivors = process.teardown();
    assert!(
        survivors.iter().any(|name| name == "sleep"),
        "teardown named {survivors:?}"
    );
    sandbox.assert_gone(child, "teardown after exit");
}

/// Dropping a process that was never waited for ends it and its children.
pub fn drop_tears_down(provider: &dyn SandboxProvider, setup: &Setup) {
    let sandbox = Sandbox::ensure(provider, setup, "drop");
    let mut process = sandbox.spawn("sleep 300 & echo $!; wait");
    let (child, reader) = first_pid(process.as_mut());
    drop(reader);
    drop(process);
    sandbox.assert_gone(child, "drop");
}

/// `stop` ends processes still running in the sandbox. Their descendants
/// cannot be probed from a stopped sandbox; providers check that in their
/// own tests.
pub fn stop_ends_processes(provider: &dyn SandboxProvider, setup: &Setup) {
    let sandbox = Sandbox::ensure(provider, setup, "stop");
    let mut process = sandbox.spawn("sleep 300 & echo $!; wait");
    let (_child, _reader) = first_pid(process.as_mut());
    provider.stop(&sandbox.name).expect("stop");
    let status = process.wait().expect("wait after stop");
    assert!(!status.success(), "a stopped process succeeded: {status}");
}

/// The port the confined process's listener takes in its own namespace.
const CONFINED_PORT: u16 = 3128;

/// A provider that declares [`crate::Capabilities::egress`] starts a
/// process that cannot reach a listener on this host's loopback, but
/// reaches the listener [`SandboxProvider::exec_confined`] hands back. One
/// that does not declare it refuses `exec_confined` as unsupported, never
/// running the process unconfined.
pub fn egress_confinement(provider: &dyn SandboxProvider, setup: &Setup) {
    use std::net::TcpListener;
    use std::sync::mpsc;

    let sandbox = Sandbox::ensure(provider, setup, "egress");
    let declared = provider.capabilities().egress;
    let outside = TcpListener::bind("127.0.0.1:0").expect("bind a host listener");
    let outside_port = outside.local_addr().expect("its address").port();
    let code = format!(
        "import socket\n\
         def reach(port):\n\
         \x20   try:\n\
         \x20       return socket.create_connection((\"127.0.0.1\", port), timeout=5)\n\
         \x20   except OSError:\n\
         \x20       return None\n\
         print(\"outside-reached\" if reach({outside_port}) else \"outside-blocked\", flush=True)\n\
         s = reach({CONFINED_PORT})\n\
         if s:\n\
         \x20   s.sendall(b\"through the listener\\n\")\n\
         \x20   s.close()\n\
         \x20   print(\"listener-reached\", flush=True)\n"
    );
    let mut exec = setup.exec("");
    exec.argv = vec!["python3".into(), "-c".into(), code];
    let confined = provider.exec_confined(&sandbox.name, &exec, CONFINED_PORT);
    if !declared {
        match confined {
            Err(ProviderError::Unsupported(unsupported)) => {
                assert_eq!(unsupported.operation, crate::Operation::Egress);
                return;
            }
            Err(other) => panic!("exec_confined without egress: expected unsupported, got {other}"),
            Ok(_) => panic!("exec_confined ran a process the provider says it cannot confine"),
        }
    }
    let (mut process, listener) = confined.unwrap_or_else(|e| panic!("exec_confined: {e}"));
    drop(process.take_stdin());
    let (sender, received) = mpsc::channel();
    thread::spawn(move || {
        let read = listener.accept().map(|(mut stream, _)| {
            let mut text = String::new();
            let _ = stream.read_to_string(&mut text);
            text
        });
        let _ = sender.send(read);
    });
    let out = drain(process.take_stdout().expect("stdout is piped"));
    let err = drain(process.take_stderr().expect("stderr is piped"));
    let through = received
        .recv_timeout(setup.timeout)
        .unwrap_or_else(|_| panic!("nothing reached the confined process's listener"))
        .expect("accept on the confined listener");
    let status = process.wait().expect("wait");
    let (out, err) = (out.join().unwrap(), err.join().unwrap());
    assert_eq!(through, "through the listener\n");
    assert!(
        status.success(),
        "the confined script failed: {status}, {err}"
    );
    assert_eq!(
        out.lines().collect::<Vec<_>>(),
        ["outside-blocked", "listener-reached"],
        "stderr: {err}"
    );
    outside.set_nonblocking(true).expect("nonblocking");
    assert!(
        outside.accept().is_err(),
        "a confined process reached this host's loopback"
    );
}
