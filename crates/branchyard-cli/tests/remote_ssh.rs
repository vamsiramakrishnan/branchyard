//! `by --remote ssh://` end to end, hermetic: the system `ssh` is replaced
//! by `crates/branchyard-recipe/tests/fixtures/fake-ssh`, which runs each
//! "remote" command on this machine (in a session of its own, with a
//! separate remote HOME), keeps a control master as a background process
//! and forwards a local Unix socket to a remote one with a small proxy.
//! So the whole path runs: the master, the remote `by serve --listen-unix`
//! started with nohup, the token generated there and read back over the
//! channel, the forward, and `run` (with the fake ACP agent), `ls` and
//! `log` through the forwarded socket; then `by remote ssh status|stop`.
//! No sshd is involved: none is installed here (docs/remote-ssh.md says
//! what a real one would add). Requires `git`, `sh`, `python3`.

#![allow(clippy::let_underscore_must_use, clippy::unwrap_used)] // tests: a panic is the failure report
use branchyard_testkit::fake_agent;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const BY: &str = env!("CARGO_BIN_EXE_by");
const FAKE_SSH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../branchyard-recipe/tests/fixtures/fake-ssh"
);

struct World {
    _dir: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
    remote_home: PathBuf,
    local_home: PathBuf,
    state: PathBuf,
    ssh: PathBuf,
    log: PathBuf,
}

impl World {
    fn new() -> World {
        fake_agent!();
        let dir = tempfile::Builder::new().prefix("by-ssh").tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let repo = root.join("srv/app");
        let remote_home = root.join("remote-home");
        let local_home = root.join("local-home");
        let state = root.join("st");
        for d in [&repo, &remote_home, &local_home] {
            fs::create_dir_all(d).unwrap();
        }
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.name", "Test"],
            &["config", "user.email", "test@localhost"],
        ] {
            assert!(git(&repo).args(args).status().unwrap().success());
        }
        fs::write(repo.join("a.txt"), "one\n").unwrap();
        assert!(git(&repo).args(["add", "."]).status().unwrap().success());
        assert!(git(&repo)
            .args(["commit", "-q", "-m", "initial"])
            .status()
            .unwrap()
            .success());
        let log = root.join("ssh.log");
        let ssh = root.join("ssh");
        fs::write(
            &ssh,
            format!(
                "#!/bin/sh\nFAKE_SSH_HOME='{}' FAKE_SSH_HOSTS=build.test FAKE_SSH_LOG='{}' \
                 exec python3 '{FAKE_SSH}' \"$@\"\n",
                remote_home.display(),
                log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
        World {
            _dir: dir,
            root,
            repo,
            remote_home,
            local_home,
            state,
            ssh,
            log,
        }
    }

    fn url(&self) -> String {
        format!("ssh://me@build.test:2222{}", self.repo.display())
    }

    fn by(&self, args: &[&str]) -> Output {
        branchyard_testkit::hermetic(&mut Command::new(BY))
            .current_dir(&self.root)
            .args(args)
            .env("HOME", &self.local_home)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("NO_COLOR", "1")
            .env(
                "BRANCHYARD_USER_CONFIG",
                "/nonexistent/branchyard-config.toml",
            )
            .env("BRANCHYARD_SSH", &self.ssh)
            .env("BRANCHYARD_SSH_BY", BY)
            .env("BRANCHYARD_SSH_DIR", &self.state)
            .env(
                "BRANCHYARD_SSH_SERVE_ARGS",
                "--allow-client-commands --quiet --shutdown-grace 5",
            )
            .env_remove("XDG_RUNTIME_DIR")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn remote(&self, args: &[&str]) -> Output {
        let url = self.url();
        let mut all = vec!["--remote", url.as_str()];
        all.extend_from_slice(args);
        self.by(&all)
    }

    fn remote_dir(&self) -> PathBuf {
        let base = self.remote_home.join(".branchyard/remote");
        let entries: Vec<_> = fs::read_dir(&base)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(entries.len(), 1, "{entries:?}");
        entries[0].clone()
    }

    fn local_dir(&self) -> PathBuf {
        let entries: Vec<_> = fs::read_dir(&self.state)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(entries.len(), 1, "{entries:?}");
        entries[0].clone()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        // Whatever a failed test left: the remote server, the master and
        // its forward.
        let _ = self.by(&["remote", "ssh", "stop", &self.url()]);
        if let Ok(base) = fs::read_dir(self.remote_home.join(".branchyard/remote")) {
            for entry in base.flatten() {
                if let Ok(pid) = fs::read_to_string(entry.path().join("run/by.pid")) {
                    let _ = Command::new("kill").args(["-9", pid.trim()]).status();
                }
            }
        }
    }
}

fn git(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    command
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn by_remote_ssh_starts_a_server_forwards_its_socket_and_runs_commands() {
    let world = World::new();
    let agent = fake_agent!().display().to_string();
    let out = world.remote(&[
        "run",
        "WRITE hello.txt=hi",
        "--name",
        "hello",
        "--harness",
        "gemini-cli",
        "--command",
        &agent,
        "--yes",
    ]);
    assert!(
        out.status.success(),
        "{}\n{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    assert!(
        text(&out.stdout).contains("wrote hello.txt"),
        "{}",
        text(&out.stdout)
    );
    assert!(
        text(&out.stderr).contains("remote mode on unix:"),
        "{}",
        text(&out.stderr)
    );

    // The remote: a private directory, a 0600 token, a server on a 0600
    // socket in a 0700 run directory, and nothing listening on TCP.
    let remote = world.remote_dir();
    assert_eq!(mode(&remote), 0o700);
    assert_eq!(mode(&remote.join("run")), 0o700);
    assert_eq!(mode(&remote.join("token")), 0o600);
    assert_eq!(mode(&remote.join("run/by.sock")), 0o600);
    let token = fs::read_to_string(remote.join("token")).unwrap();
    let pid = fs::read_to_string(remote.join("run/by.pid")).unwrap();
    // Here: the same token in a 0600 file in a 0700 directory, and the
    // forwarded socket.
    let local = world.local_dir();
    assert_eq!(mode(&local), 0o700);
    assert_eq!(mode(&local.join("token")), 0o600);
    assert_eq!(
        fs::read_to_string(local.join("token")).unwrap().trim(),
        token.trim()
    );
    assert!(local.join("by.sock").exists());
    // ssh ran as asked, and the token was never on a command line.
    let log = fs::read_to_string(&world.log).unwrap();
    assert!(!log.contains(token.trim()), "the token reached an argv");
    let masters = log.lines().filter(|l| l.contains("\"-M\"")).count();
    assert_eq!(masters, 1, "{log}");
    assert!(log.contains("ControlPersist=600"), "{log}");
    assert!(
        log.contains("\"-p\", \"2222\", \"-l\", \"me\", \"--\", \"build.test\""),
        "{log}"
    );
    assert!(log.contains("\"-O\", \"forward\", \"-L\""), "{log}");

    // Later commands reuse the master and the server.
    let out = world.remote(&["ls"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("hello"), "{}", text(&out.stdout));
    let out = world.remote(&["log", "hello"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains("hello.txt"),
        "{}",
        text(&out.stdout)
    );
    let out = world.remote(&["diff", "hello"]);
    assert!(text(&out.stdout).contains("+hi"), "{}", text(&out.stdout));
    let log = fs::read_to_string(&world.log).unwrap();
    assert_eq!(log.lines().filter(|l| l.contains("\"-M\"")).count(), 1);
    assert_eq!(
        fs::read_to_string(remote.join("run/by.pid")).unwrap(),
        pid,
        "the server was reused"
    );

    // Status, then stop.
    let out = world.by(&["remote", "ssh", "status", &world.url()]);
    let status = text(&out.stdout);
    assert!(status.contains("connection  up"), "{status}");
    assert!(
        status.contains(&format!("server      running (pid {})", pid.trim())),
        "{status}"
    );
    let out = world.remote(&["remote", "ssh", "status", "--json"]);
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["server"], "running");
    assert_eq!(value["connection"], "up");
    let out = world.remote(&["remote", "ssh", "stop"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains("stopped by serve"),
        "{}",
        text(&out.stdout)
    );
    assert!(!remote.join("run/by.sock").exists());
    assert!(!local.join("token").exists() && !local.join("by.sock").exists());
    let out = world.by(&["remote", "ssh", "status", &world.url()]);
    assert!(
        text(&out.stdout).contains("connection  down"),
        "{}",
        text(&out.stdout)
    );
    // The server's work survives it: a new connection starts a new server
    // over the same repository and token.
    let out = world.remote(&["ls"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("hello"));
    assert_eq!(fs::read_to_string(remote.join("token")).unwrap(), token);
}

#[test]
fn ssh_remotes_refuse_what_they_cannot_use_and_say_why() {
    let world = World::new();
    let out = world.by(&["--remote", &world.url(), "--token-file", "/tmp/t", "ls"]);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("fetches its own token"),
        "{}",
        text(&out.stderr)
    );
    let out = world.by(&["--remote", "ssh://elsewhere.test/srv/app", "ls"]);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("Could not resolve hostname elsewhere.test"),
        "{}",
        text(&out.stderr)
    );
    let url = format!("ssh://build.test{}", world.root.join("missing").display());
    let out = world.by(&["--remote", &url, "ls"]);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("no directory"),
        "{}",
        text(&out.stderr)
    );
    let out = world.by(&["--remote", "ssh://-oProxyCommand=x/r", "ls"]);
    assert!(
        text(&out.stderr).contains("unusable host"),
        "{}",
        text(&out.stderr)
    );
    let out = world.by(&["remote", "ssh", "status", "http://127.0.0.1:1"]);
    assert!(
        text(&out.stderr).contains("not an ssh:// remote"),
        "{}",
        text(&out.stderr)
    );
    let _ = world.by(&["remote", "ssh", "stop", &url]);
}

/// `ssh -O forward` answers only once the local socket accepts, as
/// OpenSSH's mux master does; the fake once answered as soon as the file
/// existed (after bind, before listen), which `by` could race under load.
#[test]
fn the_fake_forward_answers_only_once_its_socket_accepts() {
    use std::io::{Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    let dir = tempfile::tempdir().unwrap();
    let dir = fs::canonicalize(dir.path()).unwrap();
    let (control, local, remote) = (
        dir.join("ctl"),
        dir.join("local.sock"),
        dir.join("remote.sock"),
    );
    let upstream = UnixListener::bind(&remote).unwrap();
    let ssh = |args: &[&str]| {
        Command::new("python3")
            .arg(FAKE_SSH)
            .args(["-S", control.to_str().unwrap()])
            .args(args)
            .env("FAKE_SSH_LISTEN_DELAY", "1")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    assert!(ssh(&["-M", "-N", "-f", "host"]).status.success());
    let spec = format!("{}:{}", local.display(), remote.display());
    let out = ssh(&["-O", "forward", "-L", &spec, "host"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    // Straight away, as `by` does.
    let mut client = UnixStream::connect(&local).expect("the forward accepts");
    client.write_all(b"ping").unwrap();
    let (mut accepted, _) = upstream.accept().unwrap();
    let mut buf = [0; 4];
    accepted.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"ping");
    assert!(ssh(&["-O", "exit", "host"]).status.success());
}

/// After `-O exit` returns, the old forward is gone and a new forward of
/// the same path keeps its socket: the fake once let the old proxy, still
/// exiting, remove the new one's socket.
#[test]
fn a_forward_made_again_after_exit_keeps_its_socket() {
    use std::os::unix::net::{UnixListener, UnixStream};
    let dir = tempfile::tempdir().unwrap();
    let dir = fs::canonicalize(dir.path()).unwrap();
    let (control, local, remote) = (
        dir.join("ctl"),
        dir.join("local.sock"),
        dir.join("remote.sock"),
    );
    let _upstream = UnixListener::bind(&remote).unwrap();
    let ssh = |args: &[&str]| {
        let out = Command::new("python3")
            .arg(FAKE_SSH)
            .args(["-S", control.to_str().unwrap()])
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
    };
    let spec = format!("{}:{}", local.display(), remote.display());
    ssh(&["-M", "-N", "-f", "host"]);
    ssh(&["-O", "forward", "-L", &spec, "host"]);
    let state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&control).unwrap()).unwrap();
    let old = state["forwards"][local.to_str().unwrap()]["pid"]
        .as_i64()
        .unwrap();
    ssh(&["-O", "exit", "host"]);
    ssh(&["-M", "-N", "-f", "host"]);
    ssh(&["-O", "forward", "-L", &spec, "host"]);
    // Wait until the old proxy has certainly exited (it is not our child,
    // so poll /proc), then the new socket must still be there.
    let proc = PathBuf::from(format!("/proc/{old}"));
    let gone = || {
        fs::read_to_string(proc.join("stat"))
            .map(|s| {
                s.rsplit(')')
                    .next()
                    .unwrap_or("")
                    .trim_start()
                    .starts_with('Z')
            })
            .unwrap_or(true)
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !gone() {
        assert!(
            std::time::Instant::now() < deadline,
            "the old proxy lives on"
        );
        std::thread::yield_now();
    }
    UnixStream::connect(&local).expect("the new forward keeps its socket");
    ssh(&["-O", "exit", "host"]);
}
