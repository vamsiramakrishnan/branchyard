//! The `branchyard-bridge` binary over real TCP: authentication, exec,
//! files and trees, attempt state that survives a restart and resists
//! tampering, TLS, reaping orphans, signals, and running execs as another
//! user. Hermetic: the bridge runs on this host and listens on loopback.
//! Requires `sh` and `sleep`; the tests of process 1 and of another user
//! run only as root, and the first also needs `unshare`.

use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use branchyard_bridge::{Claims, ClientTls, Endpoint, Signer};
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
    /// Arguments added to `serve`.
    extra: Vec<String>,
    /// A command the bridge runs under, such as `unshare`.
    wrapper: Vec<String>,
    tls: Option<ClientTls>,
}

impl Bridge {
    fn start() -> Bridge {
        Bridge::start_with(Vec::new(), Vec::new(), None)
    }

    fn start_with(extra: Vec<String>, wrapper: Vec<String>, tls: Option<ClientTls>) -> Bridge {
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
            extra,
            wrapper,
            tls,
        };
        bridge.spawn();
        bridge
    }

    fn spawn(&mut self) {
        let bridge = env!("CARGO_BIN_EXE_branchyard-bridge");
        let mut command = match self.wrapper.split_first() {
            None => Command::new(bridge),
            Some((program, args)) => {
                let mut command = Command::new(program);
                command.args(args).arg(bridge);
                command
            }
        };
        let mut child = command
            .args(["serve", "--listen", "127.0.0.1:0", "--lifeline-stdin"])
            .args(&self.extra)
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
        let scheme = if self.tls.is_some() { "https" } else { "http" };
        self.url = format!("{scheme}://{address}/");
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
        let endpoint = Endpoint::new(&self.url, signer.sign(claims).unwrap()).unwrap();
        match &self.tls {
            Some(tls) => endpoint.with_tls(tls.clone()),
            None => endpoint,
        }
    }

    fn state_file(&self) -> PathBuf {
        self.dir.join("state/attempts")
    }

    /// The bridge's own PID: the child, or the wrapper's child.
    fn pid(&self) -> u32 {
        let child = self.child.as_ref().unwrap().id();
        if self.wrapper.is_empty() {
            return child;
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(pid) = children_of(child).first() {
                return *pid;
            }
            assert!(Instant::now() < deadline, "the wrapper started nothing");
            thread::sleep(Duration::from_millis(10));
        }
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

fn root() -> bool {
    // SAFETY: plain syscall.
    unsafe { libc::geteuid() == 0 }
}

/// The parent PID and state of `pid`, from `/proc`.
fn stat_of(pid: u32) -> Option<(u32, String)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let mut fields = stat.rsplit(')').next()?.split_whitespace();
    let state = fields.next()?.to_owned();
    Some((fields.next()?.parse().ok()?, state))
}

fn children_of(parent: u32) -> Vec<u32> {
    let mut children = Vec::new();
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse().ok()) else {
            continue;
        };
        if stat_of(pid).is_some_and(|(ppid, _)| ppid == parent) {
            children.push(pid);
        }
    }
    children
}

fn zombies_of(parent: u32) -> Vec<u32> {
    children_of(parent)
        .into_iter()
        .filter(|pid| stat_of(*pid).is_some_and(|(_, state)| state == "Z"))
        .collect()
}

/// A self-signed certificate authority.
fn authority() -> (String, rcgen::Issuer<'static, rcgen::KeyPair>) {
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(Vec::new()).unwrap();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let cert = params.self_signed(&key).unwrap();
    (cert.pem(), rcgen::Issuer::new(params, key))
}

/// A server certificate for loopback from `ca`, written to `dir`; returns
/// the certificate and key paths.
fn server_identity(ca: &rcgen::Issuer<'_, rcgen::KeyPair>, dir: &Path) -> (PathBuf, PathBuf) {
    let key = rcgen::KeyPair::generate().unwrap();
    let params =
        rcgen::CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()]).unwrap();
    let cert = params.signed_by(&key, ca).unwrap();
    fs::create_dir_all(dir).unwrap();
    let (cert_path, key_path) = (dir.join("bridge.crt"), dir.join("bridge.key"));
    fs::write(&cert_path, cert.pem()).unwrap();
    fs::write(&key_path, key.serialize_pem()).unwrap();
    (cert_path, key_path)
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "branchyard-bridge-test-{}-{}-{name}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn the_bridge_serves_tls_and_a_client_trusting_another_authority_is_refused() {
    let (ca_pem, ca) = authority();
    let pki = scratch("pki");
    let (cert, key) = server_identity(&ca, &pki);
    let tls = ClientTls::from_ca_pem(ca_pem.as_bytes()).unwrap();
    let bridge = Bridge::start_with(
        vec![
            "--tls-cert".into(),
            cert.display().to_string(),
            "--tls-key".into(),
            key.display().to_string(),
        ],
        Vec::new(),
        Some(tls),
    );
    assert!(bridge.url.starts_with("https://"), "{}", bridge.url);
    let endpoint = bridge.endpoint(&bridge.claims(1));
    endpoint.health().unwrap();
    let (code, out, _) = run(
        &endpoint,
        &bridge.work(),
        "echo secure; head -c 200000 /dev/zero",
    );
    assert_eq!(code, Some(0));
    assert_eq!(out.len(), "secure\n".len() + 200_000);
    let file = bridge.work().join("over-tls.bin");
    let content = vec![7u8; 300_000];
    endpoint
        .put_file(&file, 0o600, &mut content.as_slice())
        .unwrap();
    let mut back = Vec::new();
    endpoint.get_file(&file, &mut back).unwrap();
    assert_eq!(back, content);

    // Nothing in the clear.
    let plain = Endpoint::new(&bridge.url.replacen("https", "http", 1), "").unwrap();
    assert!(!plain.healthy());

    // A client that trusts another authority, or the public roots, refuses
    // the bridge before sending anything.
    let (other_pem, _) = authority();
    let wrong = endpoint
        .clone()
        .with_tls(ClientTls::from_ca_pem(other_pem.as_bytes()).unwrap());
    let error = wrong.health().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{error}");
    assert!(error.to_string().contains("certificate"), "{error}");
    assert!(wrong.exec(&sh(&bridge.work(), "true")).is_err());
    let public = Endpoint::new(&bridge.url, "").unwrap();
    assert_eq!(
        public.health().unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    let _ = fs::remove_dir_all(pki);
}

#[test]
fn orphans_of_an_exec_are_reparented_to_the_bridge_and_reaped() {
    let bridge = Bridge::start();
    let endpoint = bridge.endpoint(&bridge.claims(1));
    // The launched shell starts a shell that starts `sleep` in the
    // background and exits, then exits itself: `sleep` is orphaned.
    let mut process = endpoint
        .exec(&sh(
            &bridge.work(),
            "sh -c 'sleep 1 >/dev/null 2>&1 & echo $!'; exit 0",
        ))
        .unwrap();
    let mut out = String::new();
    process
        .take_stdout()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    assert!(process.wait().unwrap().success());
    let orphan: u32 = out.trim().parse().unwrap();
    let bridge_pid = bridge.pid();
    if let Some((parent, _)) = stat_of(orphan) {
        assert_eq!(parent, bridge_pid, "the orphan went elsewhere");
    }
    // It exits on its own and is reaped: no zombie is left.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Path::new(&format!("/proc/{orphan}")).exists() {
        assert!(
            Instant::now() < deadline,
            "orphan {orphan} was never reaped"
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert!(zombies_of(bridge_pid).is_empty());
    drop(process);
}

#[test]
fn sigterm_is_forwarded_to_the_execs_and_the_bridge_exits_cleanly() {
    let mut bridge = Bridge::start();
    let endpoint = bridge.endpoint(&bridge.claims(1));
    let mut process = endpoint
        .exec(&sh(
            &bridge.work(),
            "trap 'echo terminated; exit 7' TERM; echo ready; while :; do sleep 0.05; done",
        ))
        .unwrap();
    let mut stdout = BufReader::new(process.take_stdout().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    assert_eq!(line, "ready\n");
    // Processes the bridge starts do not inherit its blocked signals.
    let mut cat = endpoint
        .exec(&ExecSpec {
            argv: vec!["cat".into(), "/proc/self/status".into()],
            cwd: bridge.work(),
            env: Default::default(),
        })
        .unwrap();
    let mut status = String::new();
    cat.take_stdout()
        .unwrap()
        .read_to_string(&mut status)
        .unwrap();
    let blocked = status
        .lines()
        .find_map(|line| line.strip_prefix("SigBlk:"))
        .unwrap()
        .trim();
    assert_eq!(blocked.trim_start_matches('0'), "", "{blocked}");
    drop(cat);

    let pid = bridge.pid();
    // SAFETY: plain syscall.
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGTERM) }, 0);
    line.clear();
    stdout.read_line(&mut line).unwrap();
    assert_eq!(line, "terminated\n");
    let mut child = bridge.child.take().unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "{status}");
    drop(process);
}

#[test]
fn as_process_1_it_reaps_orphans_and_only_the_runtime_can_stop_it() {
    let usable = root()
        && Command::new("unshare")
            .args(["--pid", "--fork", "--mount-proc", "true"])
            .status()
            .is_ok_and(|s| s.success());
    if !usable {
        eprintln!("skipped: needs root and unshare with PID namespaces");
        return;
    }
    let mut bridge = Bridge::start_with(
        Vec::new(),
        ["unshare", "--pid", "--fork", "--mount-proc", "--kill-child"]
            .map(String::from)
            .to_vec(),
        None,
    );
    let endpoint = bridge.endpoint(&bridge.claims(1));
    let (_, parent, _) = run(&endpoint, &bridge.work(), "echo $PPID");
    assert_eq!(parent, "1\n");

    // An orphan is reparented to process 1, the bridge, and reaped.
    let mut process = endpoint
        .exec(&sh(
            &bridge.work(),
            "sh -c 'sleep 1 >/dev/null 2>&1 & echo $!'; exit 0",
        ))
        .unwrap();
    let mut out = String::new();
    process
        .take_stdout()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    assert!(process.wait().unwrap().success());
    let orphan = out.trim().to_owned();
    let (_, parent, _) = run(
        &endpoint,
        &bridge.work(),
        &format!("awk '{{print $4}}' /proc/{orphan}/stat 2>/dev/null || echo 1"),
    );
    assert_eq!(parent, "1\n");
    let zombies =
        "for s in /proc/[0-9]*/stat; do awk '$3 == \"Z\" {print $1}' $s; done 2>/dev/null";
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (_, gone, _) = run(
            &endpoint,
            &bridge.work(),
            &format!("test -e /proc/{orphan} && echo no || echo yes"),
        );
        if gone == "yes\n" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "orphan {orphan} was never reaped"
        );
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(run(&endpoint, &bridge.work(), zombies).1, "");
    drop(process);

    // Signals from inside the sandbox do not stop it; SIGKILL from there
    // never reaches process 1.
    run(
        &endpoint,
        &bridge.work(),
        "kill -TERM 1; kill -INT 1; kill -KILL 1; sleep 0.3",
    );
    assert_eq!(run(&endpoint, &bridge.work(), "echo still").1, "still\n");

    // SIGTERM from outside, as the container runtime sends it, does.
    let pid = bridge.pid();
    // SAFETY: plain syscall.
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGTERM) }, 0);
    let mut child = bridge.child.take().unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "{status}");
}

#[test]
fn execs_and_files_run_as_another_user_who_cannot_reach_the_bridge() {
    if !root() {
        eprintln!("skipped: needs root to run execs as another user");
        return;
    }
    let bridge = Bridge::start_with(
        vec!["--run-as".into(), "65534:65534".into()],
        Vec::new(),
        None,
    );
    fs::set_permissions(bridge.work(), fs::Permissions::from_mode(0o777)).unwrap();
    let endpoint = bridge.endpoint(&bridge.claims(1));
    let (code, out, _) = run(&endpoint, &bridge.work(), "id -u; id -g; id -G");
    assert_eq!(code, Some(0));
    assert_eq!(out, "65534\n65534\n65534\n");

    // The attempt state is private to the bridge's user.
    let state = bridge.state_file();
    let before = fs::read_to_string(&state).unwrap();
    assert_eq!(
        fs::metadata(state.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&state).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let script = format!(
        "cat {0}; printf 'current 0\\nended 0\\n' > {0}; rm -f {0}",
        state.display()
    );
    assert_ne!(run(&endpoint, &bridge.work(), &script).0, Some(0));
    assert_eq!(fs::read_to_string(&state).unwrap(), before);

    // Nor can it signal the bridge or read its memory or environment.
    let pid = bridge.pid();
    for probe in [
        format!("kill -0 {pid}"),
        format!("cat /proc/{pid}/environ"),
        format!("head -c 1 /proc/{pid}/mem"),
    ] {
        assert_ne!(run(&endpoint, &bridge.work(), &probe).0, Some(0), "{probe}");
    }

    // Files are written as that user, and only where it may write.
    let owned = bridge.work().join("owned.txt");
    endpoint.put_file(&owned, 0o644, &mut &b"x"[..]).unwrap();
    let meta = fs::metadata(&owned).unwrap();
    assert_eq!((meta.uid(), meta.gid()), (65534, 65534));
    let denied = endpoint.put_file(&state.with_file_name("planted"), 0o644, &mut &b"x"[..]);
    assert_eq!(denied.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(
        endpoint
            .get_file(&state, &mut Vec::new())
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    // A link the user plants is followed only with the user's rights.
    let target = bridge.work().join("target");
    std::os::unix::fs::symlink(&state, bridge.work().join("target.branchyard-partial")).unwrap();
    assert!(endpoint
        .put_file(&target, 0o644, &mut &b"current 0\n"[..])
        .is_err());
    assert_eq!(fs::read_to_string(&state).unwrap(), before);
}

#[test]
fn a_state_file_reset_behind_the_bridge_is_noticed_and_undone() {
    let mut bridge = Bridge::start();
    let first = bridge.endpoint(&bridge.claims(10));
    assert_eq!(run(&first, &bridge.work(), "true").0, Some(0));
    first.end_attempt().unwrap();
    let state = bridge.state_file();
    let before = fs::read_to_string(&state).unwrap();

    // A process of the bridge's own user resets the file.
    fs::write(&state, "current 0\nended 0\n").unwrap();
    // Memory is the authority: the ended attempt stays ended, the file is
    // restored and the tampering reported.
    assert!(refused(&first).contains("ended"));
    assert_eq!(fs::read_to_string(&state).unwrap(), before);
    let second = bridge.endpoint(&bridge.claims(11));
    assert!(second.status().unwrap().tampered);
    // So a restart revives nothing either.
    bridge.restart();
    let first = bridge.endpoint(&bridge.claims(10));
    assert!(refused(&first).contains("ended"));
    let third = bridge.endpoint(&bridge.claims(12));
    assert!(!third.status().unwrap().tampered);
}

#[test]
fn status_names_the_running_execs_and_what_they_run() {
    let bridge = Bridge::start();
    let endpoint = bridge.endpoint(&bridge.claims(3));
    assert!(endpoint.status().unwrap().execs.is_empty());
    let mut process = endpoint
        .exec(&sh(&bridge.work(), "sleep 300 & echo $!; wait"))
        .unwrap();
    let mut line = String::new();
    let mut stdout = BufReader::new(process.take_stdout().unwrap());
    stdout.read_line(&mut line).unwrap();
    let sleeper: u32 = line.trim().parse().unwrap();
    wait_exec(sleeper, "sleep");
    let status = endpoint.status().unwrap();
    assert!(!status.tampered);
    assert_eq!(status.execs.len(), 1, "{status:?}");
    let exec = &status.execs[0];
    assert_eq!(exec.pid.to_string(), process.id());
    assert_eq!(exec.attempt, 3);
    assert_eq!(exec.program, b"sh");
    assert!(exec.members.contains(&"sleep".to_owned()), "{exec:?}");

    process.teardown();
    drop(process);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !endpoint.status().unwrap().execs.is_empty() {
        assert!(Instant::now() < deadline, "the exec is still reported");
        thread::sleep(Duration::from_millis(20));
    }
}
