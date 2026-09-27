//! The `branchyard-bridge` binary over real TCP: authentication, exec,
//! files and trees, and attempt state that survives a restart. Hermetic:
//! the bridge runs on this host and listens on loopback. Requires `sh` and
//! `sleep`.

use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use branchyard_bridge::{Claims, Endpoint, Signer};
use branchyard_sandbox::{ExecSpec, Process, ProviderError};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// A bridge process with its own identity and state, stopped on drop by
/// closing its stdin.
struct Bridge {
    dir: PathBuf,
    child: Option<Child>,
    url: String,
    signer: Signer,
}

impl Bridge {
    fn start() -> Bridge {
        let dir = std::env::temp_dir().join(format!(
            "branchyard-bridge-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("identity")).unwrap();
        fs::create_dir_all(dir.join("work")).unwrap();
        for (file, value) in [("atespace", "tenant"), ("name", "actor"), ("uid", "uid-1")] {
            fs::write(dir.join("identity").join(file), format!("{value}\n")).unwrap();
        }
        let (signer, _) = Signer::generate().unwrap();
        let mut bridge = Bridge {
            dir,
            child: None,
            url: String::new(),
            signer,
        };
        bridge.spawn();
        bridge
    }

    fn spawn(&mut self) {
        let mut child = Command::new(env!("CARGO_BIN_EXE_branchyard-bridge"))
            .args(["serve", "--listen", "127.0.0.1:0", "--lifeline-stdin"])
            .arg("--identity")
            .arg(self.dir.join("identity"))
            .arg("--state")
            .arg(self.dir.join("state/attempts"))
            .env("BRANCHYARD_BRIDGE_KEY", self.signer.public_key())
            .env("BRANCHYARD_BRIDGE_SECRET_LOOKING", "never passed on")
            .env("BRIDGE_BASE", "from the bridge")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let address = line.trim().strip_prefix("listening ").expect("address");
        self.url = format!("http://{address}/");
        self.child = Some(child);
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            drop(child.stdin.take());
            child.wait().unwrap();
        }
    }

    fn restart(&mut self) {
        self.stop();
        self.spawn();
    }

    fn claims(&self, seq: u64) -> Claims {
        Claims {
            atespace: "tenant".into(),
            actor: "actor".into(),
            uid: "uid-1".into(),
            attempt: format!("test#{seq}"),
            seq,
            expires: now() + 600,
        }
    }

    fn endpoint(&self, claims: &Claims) -> Endpoint {
        self.endpoint_signed(&self.signer, claims)
    }

    fn endpoint_signed(&self, signer: &Signer, claims: &Claims) -> Endpoint {
        Endpoint::new(&self.url, signer.sign(claims).unwrap()).unwrap()
    }

    fn work(&self) -> PathBuf {
        self.dir.join("work")
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.stop();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn sh(cwd: &Path, script: &str) -> ExecSpec {
    ExecSpec {
        argv: vec!["sh".into(), "-c".into(), script.into()],
        cwd: cwd.to_path_buf(),
        env: [("BY_TEST".into(), "set".into())].into(),
    }
}

fn run(endpoint: &Endpoint, cwd: &Path, script: &str) -> (Option<i32>, String, String) {
    let mut process = endpoint.exec(&sh(cwd, script)).unwrap();
    drop(process.take_stdin());
    let mut out = process.take_stdout().unwrap();
    let mut err = process.take_stderr().unwrap();
    let errors = thread::spawn(move || {
        let mut text = String::new();
        err.read_to_string(&mut text).unwrap();
        text
    });
    let mut text = String::new();
    out.read_to_string(&mut text).unwrap();
    let status = process.wait().unwrap();
    (status.code, text, errors.join().unwrap())
}

fn refused(endpoint: &Endpoint) -> String {
    match endpoint.exec(&sh(Path::new("/"), "true")) {
        Err(ProviderError::Io(error)) => {
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
            error.to_string()
        }
        Err(other) => panic!("expected a refusal, got {other}"),
        Ok(_) => panic!("the connection was accepted"),
    }
}

fn alive(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        let state = stat
            .rsplit(')')
            .next()
            .unwrap_or("")
            .split_whitespace()
            .next();
        !matches!(state, Some("Z") | Some("X"))
    })
}

/// Wait until `pid` runs `name`: a forked child is named after its parent
/// until it execs.
fn wait_exec(pid: u32, name: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default() != format!("{name}\n")
    {
        assert!(Instant::now() < deadline, "pid {pid} never ran {name}");
        thread::sleep(Duration::from_millis(5));
    }
}

fn wait_gone(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(pid) {
        assert!(Instant::now() < deadline, "pid {pid} survived");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn an_exec_relays_stdio_env_and_exit_status() {
    let bridge = Bridge::start();
    let endpoint = bridge.endpoint(&bridge.claims(1));
    let (code, out, err) = run(
        &endpoint,
        &bridge.work(),
        r#"printf '%s|%s|%s|%s' "$BY_TEST" "$BRIDGE_BASE" "${BRANCHYARD_BRIDGE_KEY-unset}" "$(pwd -P)"; echo oops >&2; exit 3"#,
    );
    assert_eq!(code, Some(3));
    assert_eq!(
        out,
        format!(
            "set|from the bridge|unset|{}",
            fs::canonicalize(bridge.work()).unwrap().display()
        )
    );
    assert_eq!(err, "oops\n");

    let mut process = endpoint.exec(&sh(&bridge.work(), "cat")).unwrap();
    let mut stdin = process.take_stdin().unwrap();
    let big = vec![b'x'; 300_000];
    let reader = {
        let mut out = process.take_stdout().unwrap();
        thread::spawn(move || {
            let mut all = Vec::new();
            out.read_to_end(&mut all).unwrap();
            all
        })
    };
    stdin.write_all(&big).unwrap();
    drop(stdin);
    assert!(process.wait().unwrap().success());
    assert_eq!(reader.join().unwrap(), big);

    let missing = ExecSpec {
        argv: vec!["/nonexistent/program".into()],
        cwd: bridge.work(),
        env: Default::default(),
    };
    assert!(matches!(
        endpoint.exec(&missing),
        Err(ProviderError::Io(e)) if e.kind() == io::ErrorKind::NotFound
    ));
}

#[test]
fn teardown_names_and_kills_what_outlived_the_process() {
    let bridge = Bridge::start();
    let endpoint = bridge.endpoint(&bridge.claims(1));
    let mut process = endpoint
        .exec(&sh(&bridge.work(), "sleep 300 & echo $!"))
        .unwrap();
    let mut line = String::new();
    BufReader::new(process.take_stdout().unwrap())
        .read_line(&mut line)
        .unwrap();
    let sleeper: u32 = line.trim().parse().unwrap();
    assert!(process.wait().unwrap().success());
    assert!(alive(sleeper));
    wait_exec(sleeper, "sleep");
    let survivors = process.teardown();
    assert_eq!(survivors, vec!["sleep".to_owned()]);
    wait_gone(sleeper);

    // Dropping a process ends its group too.
    let mut process = endpoint
        .exec(&sh(&bridge.work(), "sleep 300 & echo $!; wait"))
        .unwrap();
    let mut line = String::new();
    BufReader::new(process.take_stdout().unwrap())
        .read_line(&mut line)
        .unwrap();
    let sleeper: u32 = line.trim().parse().unwrap();
    drop(process);
    wait_gone(sleeper);
}

#[test]
fn files_and_trees_cross_both_ways() {
    let bridge = Bridge::start();
    let endpoint = bridge.endpoint(&bridge.claims(1));
    let guest = bridge.work().join("nested/file.bin");
    let content: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    endpoint
        .put_file(&guest, 0o750, &mut content.as_slice())
        .unwrap();
    assert_eq!(fs::read(&guest).unwrap(), content);
    let mut back = Vec::new();
    endpoint.get_file(&guest, &mut back).unwrap();
    assert_eq!(back, content);
    let missing = endpoint.get_file(&bridge.work().join("missing"), &mut Vec::new());
    assert_eq!(missing.unwrap_err().kind(), io::ErrorKind::NotFound);
    assert!(endpoint
        .put_file(Path::new("relative"), 0o644, &mut &b""[..])
        .is_err());

    let host = bridge.dir.join("host-tree");
    fs::create_dir_all(host.join("a/b")).unwrap();
    fs::write(host.join("a/b/c.txt"), "c").unwrap();
    std::os::unix::fs::symlink("b/c.txt", host.join("a/link")).unwrap();
    endpoint
        .put_tree(&host, &bridge.work().join("tree"))
        .unwrap();
    assert_eq!(
        fs::read_to_string(bridge.work().join("tree/a/b/c.txt")).unwrap(),
        "c"
    );
    let back = bridge.dir.join("back");
    endpoint
        .get_tree(&bridge.work().join("tree"), &back)
        .unwrap();
    assert_eq!(fs::read_to_string(back.join("a/link")).unwrap(), "c");
    assert!(fs::symlink_metadata(back.join("a/link"))
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn wrong_expired_and_foreign_credentials_are_refused() {
    let bridge = Bridge::start();
    let no_credential = Endpoint::new(&bridge.url, "").unwrap();
    assert!(refused(&no_credential).contains("no bridge credential"));
    let garbage = Endpoint::new(&bridge.url, "garbage").unwrap();
    assert!(refused(&garbage).contains("malformed"));

    let (stranger, _) = Signer::generate().unwrap();
    let forged = bridge.endpoint_signed(&stranger, &bridge.claims(1));
    assert!(refused(&forged).contains("signature"));

    let mut expired = bridge.claims(1);
    expired.expires = now() - 1;
    assert!(refused(&bridge.endpoint(&expired)).contains("expired"));

    let mut other_actor = bridge.claims(1);
    other_actor.uid = "uid-2".into();
    assert!(refused(&bridge.endpoint(&other_actor)).contains("another actor"));

    // None of the refusals started an attempt.
    let endpoint = bridge.endpoint(&bridge.claims(1));
    assert_eq!(run(&endpoint, &bridge.work(), "true").0, Some(0));
}

#[test]
fn an_ended_or_superseded_attempt_is_never_accepted_again() {
    let mut bridge = Bridge::start();
    let first = bridge.endpoint(&bridge.claims(10));
    assert_eq!(run(&first, &bridge.work(), "true").0, Some(0));

    // A process of the first attempt is torn down when it ends.
    let mut process = first
        .exec(&sh(&bridge.work(), "sleep 300 & echo $!; wait"))
        .unwrap();
    let mut line = String::new();
    let mut stdout = BufReader::new(process.take_stdout().unwrap());
    stdout.read_line(&mut line).unwrap();
    let sleeper: u32 = line.trim().parse().unwrap();
    wait_exec(sleeper, "sleep");
    let killed = first.end_attempt().unwrap();
    assert!(killed.iter().any(|name| name == "sleep"), "{killed:?}");
    wait_gone(sleeper);
    assert!(!process.wait().unwrap().success());
    assert!(refused(&first).contains("ended"));

    // A newer attempt is accepted and supersedes every older one.
    let second = bridge.endpoint(&bridge.claims(12));
    assert_eq!(run(&second, &bridge.work(), "true").0, Some(0));
    let older = bridge.endpoint(&bridge.claims(11));
    assert!(refused(&older).contains("ended"));

    // The state survives a restart: nothing is revived.
    bridge.restart();
    let first = bridge.endpoint(&bridge.claims(10));
    let second = bridge.endpoint(&bridge.claims(12));
    assert!(refused(&first).contains("ended"));
    assert_eq!(run(&second, &bridge.work(), "true").0, Some(0));

    // A branched actor gets a new UID, so the parent's credential is dead
    // there even though the attempt state was copied with the filesystem.
    fs::write(bridge.dir.join("identity/uid"), "uid-child\n").unwrap();
    assert!(refused(&second).contains("another actor"));
}

#[test]
fn the_health_check_answers_without_a_credential() {
    let bridge = Bridge::start();
    let endpoint = Endpoint::new(&bridge.url, "").unwrap();
    assert!(endpoint.healthy());
    let shut = bridge.endpoint(&bridge.claims(1)).shutdown().unwrap();
    assert!(shut.is_empty());
}
