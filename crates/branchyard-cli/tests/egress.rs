//! `--network`, `[network]`, `--permissions` and what `by show` and `by
//! log` say about egress, end to end with the built `by` and the fake ACP
//! agent. Hosts are listeners on this machine's loopback; the harness
//! probes them with Python. The enforced case needs a host that allows
//! unprivileged user and network namespaces: elsewhere it says why and
//! skips, and fails if `unshare -rn` works there. The rest runs with
//! confinement turned off (`BRANCHYARD_EGRESS_NETNS=off`). Requires `git`,
//! `sh` and `python3`.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use branchyard_testkit::fake_agent;
use serde_json::Value;

static COUNTER: AtomicU64 = AtomicU64::new(0);

const PROBE: &str = r#"import socket, sys, urllib.error, urllib.request
mode, port = sys.argv[1], int(sys.argv[2])
if mode == "proxy":
    try:
        body = urllib.request.urlopen(f"http://127.0.0.1:{port}/x", timeout=10).read()
        print(f"proxy {port}: {body.decode().strip()}")
    except urllib.error.HTTPError as error:
        print(f"proxy {port}: {error.code}")
else:
    try:
        socket.create_connection(("127.0.0.1", port), timeout=5).close()
        print(f"direct {port}: reached")
    except OSError:
        print(f"direct {port}: blocked")
"#;

/// A listener answering every request with `upstream <port>`.
fn upstream() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                let body = format!("upstream {port}\n");
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    port
}

struct Repo {
    dir: PathBuf,
    root: PathBuf,
    probe: PathBuf,
    /// `off` turns confinement off for every `by` this runs.
    netns: Option<&'static str>,
}

impl Repo {
    fn new(netns: Option<&'static str>) -> Repo {
        let dir = std::env::temp_dir().join(format!(
            "branchyard-cli-egress-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("repo")).unwrap();
        let dir = fs::canonicalize(dir).unwrap();
        let repo = Repo {
            root: dir.join("repo"),
            probe: dir.join("probe.py"),
            dir,
            netns,
        };
        fs::write(&repo.probe, PROBE).unwrap();
        repo.git(&["init", "-q", "-b", "main"]);
        repo.git(&["config", "user.name", "Test"]);
        repo.git(&["config", "user.email", "test@localhost"]);
        fs::write(repo.root.join("a.txt"), "one\n").unwrap();
        repo.git(&["add", "."]);
        repo.git(&["commit", "-q", "-m", "initial"]);
        repo
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(&self.root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("NO_COLOR", "1")
            .env(
                "BRANCHYARD_USER_CONFIG",
                "/nonexistent/branchyard-config.toml",
            );
        for var in [
            "BRANCHYARD_DELEGATION",
            "BRANCHYARD_BRANCH",
            "BRANCHYARD_ROOT",
            "BRANCHYARD_BY",
            "BRANCHYARD_EGRESS_NETNS",
        ] {
            command.env_remove(var);
        }
        if let Some(value) = self.netns {
            command.env("BRANCHYARD_EGRESS_NETNS", value);
        }
        command
    }

    fn git(&self, args: &[&str]) -> String {
        let out = self.command("git").args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    fn by(&self, args: &[&str]) -> Output {
        self.command(env!("CARGO_BIN_EXE_by"))
            .args(args)
            .output()
            .unwrap()
    }

    /// `by run` on the fake agent with `flags`, the prompt running the
    /// probe once per `(mode, port)`.
    fn run(&self, name: &str, flags: &[&str], probes: &[(&str, u16)]) -> Output {
        let prompt: Vec<String> = probes
            .iter()
            .map(|(mode, port)| format!("SH python3 {} {mode} {port}", self.probe.display()))
            .collect();
        let agent = fake_agent!().display().to_string();
        let mut args = vec![
            "run",
            "--name",
            name,
            "--harness",
            "gemini-cli",
            "--command",
            &agent,
        ];
        args.extend(flags);
        let prompt = prompt.join("\n");
        args.extend(["--", &prompt]);
        self.by(&args)
    }

    fn json(&self, args: &[&str]) -> Value {
        let out = self.by(args);
        assert!(out.status.success(), "{args:?}: {}", stderr(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Why this host cannot confine a harness, when that is the kernel's
/// doing: `unshare -rn` (util-linux) makes the same request. Where it
/// works, `by` must confine, and the test's required policy fails if it
/// does not.
fn unavailable() -> Option<String> {
    let unshare = Command::new("unshare").args(["-rn", "true"]).status();
    match unshare {
        Ok(status) if status.success() => None,
        Ok(status) => Some(format!("unshare -rn true failed: {status}")),
        Err(error) => Some(format!("unshare could not run: {error}")),
    }
}

#[test]
fn an_enforced_policy_is_shown_by_show_and_log() {
    let repo = Repo::new(None);
    if let Some(why) = unavailable() {
        eprintln!("SKIPPED an_enforced_policy_is_shown_by_show_and_log: {why}");
        return;
    }
    let (allowed, denied) = (upstream(), upstream());
    let rule = format!("127.0.0.1:{allowed}");
    let out = repo.run(
        "enforced",
        &["--yes", "--network", &rule, "--network-enforce", "required"],
        &[("proxy", allowed), ("proxy", denied), ("direct", allowed)],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let said = format!("{}{}", stdout(&out), stderr(&out));
    assert!(
        said.contains(&format!("proxy {allowed}: upstream {allowed}")),
        "{said}"
    );
    assert!(said.contains(&format!("proxy {denied}: 403")), "{said}");
    assert!(
        said.contains(&format!("direct {allowed}: blocked")),
        "{said}"
    );
    assert!(
        said.contains(&format!("egress enforced: {rule} (required)")),
        "{said}"
    );

    let shown = repo.json(&["show", "enforced", "--json"]);
    assert_eq!(shown["egress"]["enforcement"], "enforced");
    assert_eq!(shown["egress"]["allowed"], 1);
    assert_eq!(shown["egress"]["denied"], 1);
    assert_eq!(shown["egress"]["allow"], serde_json::json!([rule]));
    let text = stdout(&repo.by(&["show", "enforced"]));
    assert!(
        text.contains(&format!(
            "enforced ({rule} (required)); 1 allowed, 1 denied"
        )),
        "{text}"
    );
    let log = stdout(&repo.by(&["log", "enforced", "--json"]));
    let events: Vec<Value> = serde_json::from_str(&log).unwrap();
    let egress: Vec<&Value> = events
        .iter()
        .filter(|v| v["activity"] == "egress")
        .collect();
    assert_eq!(egress.len(), 3, "{log}");
    assert_eq!(egress[0]["egress"]["kind"], "applied");
    assert_eq!(egress[1]["egress"]["allowed"], true);
    assert_eq!(egress[2]["egress"]["allowed"], false);
    let log = stdout(&repo.by(&["log", "enforced"]));
    assert!(
        log.contains(&format!(
            "egress allowed: GET 127.0.0.1:{allowed} by {rule}"
        )),
        "{log}"
    );
    assert!(
        log.contains(&format!(
            "egress denied: GET 127.0.0.1:{denied} (no rule allows it)"
        )),
        "{log}"
    );
}

#[test]
fn without_confinement_a_policy_is_advisory_or_refused() {
    let repo = Repo::new(Some("off"));
    let (allowed, denied) = (upstream(), upstream());
    // Required: refused before anything runs.
    let out = repo.run(
        "required",
        &[
            "--yes",
            "--network",
            "none",
            "--network-enforce",
            "required",
        ],
        &[("proxy", allowed)],
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("must be enforced"),
        "{}",
        stderr(&out)
    );
    assert!(!repo.by(&["show", "required"]).status.success());

    // Best effort, from branchyard.toml: runs, and says it is advisory.
    fs::write(
        repo.root.join("branchyard.toml"),
        format!("[network]\nallow = [\"127.0.0.1:{allowed}\"]\n"),
    )
    .unwrap();
    let out = repo.run(
        "advisory",
        &["--yes"],
        &[("proxy", allowed), ("proxy", denied), ("direct", denied)],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let said = format!("{}{}", stdout(&out), stderr(&out));
    assert!(
        said.contains(&format!("proxy {allowed}: upstream {allowed}")),
        "{said}"
    );
    assert!(said.contains(&format!("proxy {denied}: 403")), "{said}");
    assert!(
        said.contains(&format!("direct {denied}: reached")),
        "{said}"
    );
    assert!(said.contains("egress advisory"), "{said}");
    let text = stdout(&repo.by(&["show", "advisory"]));
    assert!(text.contains("advisory ("), "{text}");
    assert!(
        text.contains("not enforced: only tools that honor"),
        "{text}"
    );
    let shown = repo.json(&["show", "advisory", "--json"]);
    assert_eq!(shown["egress"]["enforcement"], "advisory");
    assert!(shown["egress"]["reason"]
        .as_str()
        .unwrap()
        .contains("BRANCHYARD_EGRESS_NETNS=off"));

    // A bad rule is refused by the flag's parser.
    let out = repo.by(&["run", "--network", "https://x.com", "--", "hi"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("not a URL"), "{}", stderr(&out));
    // And `by config validate` checks the file.
    fs::write(
        repo.root.join("branchyard.toml"),
        "[network]\nallow = [\"*\"]\n",
    )
    .unwrap();
    let out = repo.by(&["config", "validate"]);
    assert!(!out.status.success());
    assert!(
        format!("{}{}", stdout(&out), stderr(&out)).contains("network.allow"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn permission_presets_decide_tool_requests() {
    let repo = Repo::new(Some("off"));
    let agent = fake_agent!().display().to_string();
    let ask = |name: &str, preset: &str| {
        repo.by(&[
            "run",
            "--name",
            name,
            "--harness",
            "gemini-cli",
            "--command",
            &agent,
            "--permissions",
            preset,
            "--",
            "PERMISSION",
        ])
    };
    let full = ask("full", "full");
    assert!(full.status.success(), "{}", stderr(&full));
    assert!(stdout(&full).contains("allowed"), "{}", stdout(&full));
    let read_only = ask("read-only", "read-only");
    assert!(read_only.status.success(), "{}", stderr(&read_only));
    assert!(
        stdout(&read_only).contains("denied"),
        "{}",
        stdout(&read_only)
    );
    // Unknown presets and mixing with --yes are refused.
    let out = ask("bad", "builtin:yolo");
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("read-only, edit-worktree, full"),
        "{}",
        stderr(&out)
    );
    let out = repo.by(&["run", "--yes", "--permissions", "full", "--", "hi"]);
    assert!(!out.status.success());
    // `[defaults] permissions` takes a preset too.
    fs::write(
        repo.root.join("branchyard.toml"),
        "[defaults]\npermissions = \"read-only\"\n",
    )
    .unwrap();
    let out = repo.by(&[
        "run",
        "--name",
        "configured",
        "--harness",
        "gemini-cli",
        "--command",
        &agent,
        "--",
        "PERMISSION",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("denied"), "{}", stdout(&out));
    let shown = stdout(&repo.by(&["config", "show"]));
    assert!(shown.contains("defaults.permissions  read-only"), "{shown}");
}
