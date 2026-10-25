//! Running Anvil's gateway: the command, a supervisor that restarts it and
//! reads its audit log, and the state file of one started in the
//! background (`by gateway start`).
//!
//! The gateway is `anvil serve mcp <bundle-root> --fleet --http <port>` with
//! `ANVIL_INBOUND_AUTH_MODE=branchyard`, the yard's issuer, the gateway's
//! `/mcp` URL as audience, the yard's public keys as a `file:` (local) or
//! `https:` (server) JWKS URI, an audit file and a vault key file.

use branchyard_support::LockExt as _;
use std::fs;
use std::io::{Read as _, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::keys;
use crate::effects::mcp;
use crate::models::upstream::{self, Target};
use crate::Error;

/// What starts one gateway.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GatewayCommand {
    /// Anvil's command and any leading arguments: `["anvil"]`, or
    /// `["node", ".../bin-anvil.js"]`.
    pub anvil: Vec<String>,
    /// The bundle root it serves.
    pub bundles: PathBuf,
    /// The port it listens on, and the address (`--host`) when not
    /// loopback.
    pub port: u16,
    pub host: Option<String>,
    pub issuer: String,
    /// The gateway's canonical `/mcp` URL: tokens' `aud`.
    pub audience: String,
    /// `file:///.../jwks.json` or `https://.../.well-known/jwks.json`.
    pub jwks_uri: String,
    pub audit_file: PathBuf,
    /// A 0600 file holding the vault's 32-byte key (64 hex characters).
    pub vault_key_file: PathBuf,
    /// Where the vault keeps its encrypted records.
    pub vault_dir: Option<PathBuf>,
    /// The gateway's public base URL, for its `/connect/callback`, when it
    /// is not the audience's.
    pub public_url: Option<String>,
}

impl GatewayCommand {
    /// The arguments after the program.
    pub fn args(&self) -> Vec<String> {
        let mut args: Vec<String> = self.anvil.iter().skip(1).cloned().collect();
        args.extend([
            "serve".into(),
            "mcp".into(),
            self.bundles.display().to_string(),
            "--fleet".into(),
            "--http".into(),
            self.port.to_string(),
        ]);
        if let Some(host) = &self.host {
            args.extend(["--host".into(), host.clone()]);
        }
        args
    }

    /// The variables it is started with.
    pub fn env(&self) -> Vec<(String, String)> {
        let mut env = vec![
            (
                "ANVIL_INBOUND_AUTH_MODE".to_owned(),
                "branchyard".to_owned(),
            ),
            ("ANVIL_INBOUND_ISSUER".to_owned(), self.issuer.clone()),
            ("ANVIL_INBOUND_AUDIENCE".to_owned(), self.audience.clone()),
            ("ANVIL_INBOUND_JWKS_URI".to_owned(), self.jwks_uri.clone()),
            (
                "ANVIL_AUDIT_FILE".to_owned(),
                self.audit_file.display().to_string(),
            ),
            (
                "ANVIL_VAULT_KEY_FILE".to_owned(),
                self.vault_key_file.display().to_string(),
            ),
        ];
        if let Some(dir) = &self.vault_dir {
            env.push(("ANVIL_VAULT_DIR".into(), dir.display().to_string()));
        }
        if let Some(url) = &self.public_url {
            env.push(("ANVIL_GATEWAY_PUBLIC_URL".into(), url.clone()));
        }
        env
    }

    /// The process, its output appended to `log`. Nothing secret is on its
    /// command line or in its environment: the keys are files.
    pub fn command(&self, log: &Path) -> Result<Command, Error> {
        let program = self
            .anvil
            .first()
            .ok_or_else(|| Error::Unsupported("no anvil command is configured".into()))?;
        let out = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .map_err(|e| Error::State(format!("open {}: {e}", log.display())))?;
        let err = out
            .try_clone()
            .map_err(|e| Error::State(format!("open {}: {e}", log.display())))?;
        let mut command = Command::new(program);
        command
            .args(self.args())
            .envs(self.env())
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err);
        Ok(command)
    }
}

/// Make the vault key file if there is none: 32 random bytes as 64 hex
/// characters, mode 0600.
pub fn ensure_vault_key(path: &Path) -> Result<(), Error> {
    if path.exists() {
        return Ok(());
    }
    let mut bytes = [0u8; 32];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes)
        .map_err(|_| Error::State("could not generate a vault key".into()))?;
    keys::write_private(path, hex::encode(bytes).as_bytes())
}

/// The port of an `http://host:port/...` URL (80 or 443 when it names
/// none), and its host.
pub fn host_port(url: &str) -> Option<(String, u16)> {
    let parsed = url::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let host = match parsed.host()? {
        url::Host::Domain(domain) => domain.to_owned(),
        url::Host::Ipv4(addr) => addr.to_string(),
        url::Host::Ipv6(addr) => addr.to_string(),
    };
    Some((host, parsed.port_or_known_default()?))
}

/// Whether something accepts connections at `url`'s host and port.
pub fn listening(url: &str) -> bool {
    let Some((host, port)) = host_port(url) else {
        return false;
    };
    let Ok(addrs) = (host.as_str(), port).to_socket_addrs() else {
        return false;
    };
    addrs
        .into_iter()
        .any(|addr| TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok())
}

/// What holds the port a gateway is pinned to, as [`probe`] finds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Held {
    /// Nothing accepts connections there.
    Nothing,
    /// An MCP server over HTTP: what a gateway is.
    Gateway,
    /// Something else, and what it did instead of answering as an MCP
    /// server does.
    Other(String),
}

/// How long [`probe`] waits for the port to answer. A listener that accepts
/// and never speaks is not a gateway, and is not waited on for long.
const PROBE_QUIET: Duration = Duration::from_secs(5);

/// Ask what holds `url`'s port: nothing, an MCP server (a gateway), or
/// something else. Accepting a TCP connection does not make a gateway (a
/// database, a dev server, another repository's tools do too), so the port is
/// asked an MCP `initialize`, sent without a token. A gateway replies with
/// the initialize result (JSON or an event stream), or refuses it for want
/// of a token: a `401` with a `WWW-Authenticate` challenge, or a `401` or
/// `403` carrying a JSON-RPC error. Any other answer is not an MCP server's.
/// This tells an MCP server from something that is not one; it cannot tell
/// Anvil from another MCP server.
pub fn probe(url: &str) -> Held {
    probe_within(url, PROBE_QUIET)
}

fn probe_within(url: &str, quiet: Duration) -> Held {
    if !listening(url) {
        return Held::Nothing;
    }
    let target = match Target::parse(url) {
        Ok(target) => target,
        Err(why) => return Held::Other(format!("{url} is not a gateway URL: {why}")),
    };
    let headers = [
        ("Content-Type", "application/json"),
        ("Accept", "application/json, text/event-stream"),
        ("Accept-Encoding", "identity"),
        ("Mcp-Protocol-Version", mcp::PROTOCOL_VERSION),
    ]
    .map(|(name, value)| (name.to_owned(), value.to_owned()));
    let initialize = json!({
        "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": {
            "protocolVersion": mcp::PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "branchyard-probe", "version": env!("CARGO_PKG_VERSION")}
        }
    });
    let body = serde_json::to_vec(&initialize).unwrap_or_default();
    let mut response = match upstream::send_within(&target, "POST", "", &headers, &body, quiet) {
        Ok(response) => response,
        Err(why) => {
            return Held::Other(format!(
                "it accepts connections but did not answer an MCP initialize over HTTP ({why})"
            ))
        }
    };
    let mut answer = Vec::new();
    // A stream a server keeps open ends in a timeout, after what it sent.
    branchyard_support::best_effort(
        "read the gateway probe's answer",
        (&mut response.body)
            .take(64 * 1024)
            .read_to_end(&mut answer),
    );
    classify(
        response.status,
        response.header("www-authenticate").is_some(),
        response.header("content-type"),
        &answer,
    )
}

/// Whether an HTTP answer to an unauthenticated MCP `initialize` is an MCP
/// server's: the initialize result, or a refusal for want of a token. A
/// JSON-RPC error with a `200` is not enough: other JSON-RPC servers (a
/// blockchain node's) answer an unknown method that way.
fn classify(status: u16, challenged: bool, content_type: Option<&str>, body: &[u8]) -> Held {
    let answer = mcp::rpc_answer(content_type, body, &json!(0));
    let initialized = answer
        .as_ref()
        .and_then(|a| a.get("result"))
        .is_some_and(|r| r.get("protocolVersion").is_some() || r.get("serverInfo").is_some());
    let in_rpc = answer.is_some()
        || serde_json::from_slice::<serde_json::Value>(body)
            .is_ok_and(|v| v.get("jsonrpc").and_then(serde_json::Value::as_str) == Some("2.0"));
    let refused = (status == 401 && challenged) || (matches!(status, 401 | 403) && in_rpc);
    if initialized || refused {
        return Held::Gateway;
    }
    Held::Other(match status {
        200..=299 => "it answers HTTP, but not with an MCP initialize result".to_owned(),
        status => format!("it answered {status} to an MCP initialize"),
    })
}

/// A gateway process, restarted when it exits, with `tick` called about
/// every half second (to read its audit log). Stopped when dropped.
pub struct Supervisor {
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    thread: Option<JoinHandle<()>>,
}

impl Supervisor {
    pub fn start(
        command: GatewayCommand,
        log: PathBuf,
        tick: Box<dyn Fn() + Send>,
    ) -> Result<Supervisor, Error> {
        // Fail now, not in the loop, when it cannot even be built.
        command.command(&log)?;
        let stop = Arc::new(AtomicBool::new(false));
        let child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        let thread = {
            let (stop, child) = (stop.clone(), child.clone());
            std::thread::Builder::new()
                .name("by-gateway".into())
                .spawn(move || supervise(&command, &log, &stop, &child, tick.as_ref()))
                .map_err(|e| Error::State(format!("could not start the supervisor: {e}")))?
        };
        Ok(Supervisor {
            stop,
            child,
            thread: Some(thread),
        })
    }

    /// The running gateway's pid, if one is running.
    pub fn pid(&self) -> Option<u32> {
        self.child.lock_recovering("child").as_ref().map(Child::id)
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            branchyard_support::join_reporting("connector gateway accept", thread);
        }
    }
}

fn note(log: &Path, text: &str) {
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(log) {
        branchyard_support::best_effort(
            "append to the connector gateway log",
            writeln!(file, "branchyard: {text}"),
        );
    }
}

fn supervise(
    command: &GatewayCommand,
    log: &Path,
    stop: &AtomicBool,
    slot: &Mutex<Option<Child>>,
    tick: &dyn Fn(),
) {
    let mut backoff = Duration::from_secs(1);
    let mut next_start = Instant::now();
    let mut started = Instant::now();
    while !stop.load(Ordering::SeqCst) {
        {
            let mut held = slot.lock_recovering("slot");
            match held.as_mut().map(Child::try_wait) {
                None if Instant::now() >= next_start => {
                    match command.command(log).and_then(|mut c| {
                        c.spawn()
                            .map_err(|e| Error::State(format!("could not start anvil: {e}")))
                    }) {
                        Ok(child) => {
                            note(log, &format!("started the gateway (pid {})", child.id()));
                            started = Instant::now();
                            *held = Some(child);
                        }
                        Err(error) => {
                            note(log, &error.to_string());
                            next_start = Instant::now() + backoff;
                            backoff = (backoff * 2).min(Duration::from_secs(30));
                        }
                    }
                }
                Some(Ok(Some(status))) => {
                    note(log, &format!("the gateway exited ({status}); restarting"));
                    *held = None;
                    // A gateway that ran a while starts again at once.
                    if started.elapsed() > Duration::from_secs(60) {
                        backoff = Duration::from_secs(1);
                    }
                    next_start = Instant::now() + backoff;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
                _ => {}
            }
        }
        tick();
        std::thread::sleep(Duration::from_millis(500));
    }
    let child = slot.lock_recovering("slot").take();
    if let Some(mut child) = child {
        terminate(&mut child);
        note(log, "stopped the gateway");
    }
    tick();
}

/// SIGTERM, then SIGKILL after five seconds.
fn terminate(child: &mut Child) {
    if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
        branchyard_support::best_effort(
            "stop the gateway process",
            rustix::process::kill_process(pid, rustix::process::Signal::TERM),
        );
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    branchyard_support::best_effort("kill child", child.kill());
    branchyard_support::best_effort("reap child", child.wait());
}

/// A gateway supervisor started in the background, as its state file
/// (`.branchyard/gateway/gateway.json`) records it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Background {
    /// The supervisor's pid; it leads the process group the gateway is in.
    pub pid: u32,
    /// Its start time, so a reused pid is never mistaken for it.
    pub start: String,
    pub url: String,
    pub log: PathBuf,
    pub started_at_ms: u64,
}

/// The state file of a background gateway in `dir`.
pub fn state_file(dir: &Path) -> PathBuf {
    dir.join("gateway.json")
}

impl Background {
    /// This process, as the supervisor of a gateway at `url`.
    pub fn this_process(url: &str, log: &Path) -> Background {
        Background {
            pid: std::process::id(),
            start: crate::proc::own_start().to_owned(),
            url: url.to_owned(),
            log: log.to_path_buf(),
            started_at_ms: branchyard_support::time::now_ms(),
        }
    }

    pub fn save(&self, dir: &Path) -> Result<(), Error> {
        let text = serde_json::to_string_pretty(self).map_err(|e| Error::State(e.to_string()))?;
        keys::write_atomic(&state_file(dir), text.as_bytes(), 0o600)
    }

    /// The background gateway recorded in `dir`, if its supervisor still
    /// runs; a stale state file is removed.
    pub fn running(dir: &Path) -> Option<Background> {
        let path = state_file(dir);
        let state: Background = serde_json::from_str(&fs::read_to_string(&path).ok()?).ok()?;
        if crate::proc::alive(state.pid, &state.start) {
            return Some(state);
        }
        branchyard_support::cleanup_file(&path);
        None
    }

    /// Stop it: SIGTERM to its process group (the supervisor and the
    /// gateway), SIGKILL after five seconds; then forget it.
    pub fn stop(&self, dir: &Path) -> Result<(), Error> {
        rustix::process::Pid::from_raw(self.pid as i32)
            .ok_or_else(|| Error::State(format!("pid {} is not usable", self.pid)))?;
        branchyard_support::terminate_group(self.pid);
        let deadline = Instant::now() + Duration::from_secs(5);
        while crate::proc::alive(self.pid, &self.start) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        if crate::proc::alive(self.pid, &self.start) {
            branchyard_support::kill_group(self.pid);
        }
        branchyard_support::cleanup_file(state_file(dir));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard_testkit::{MockHttp, Response};

    fn command(anvil: &[&str]) -> GatewayCommand {
        GatewayCommand {
            anvil: anvil.iter().map(|s| (*s).to_owned()).collect(),
            bundles: "/srv/bundles".into(),
            port: 8931,
            host: None,
            issuer: "branchyard:local:y".into(),
            audience: "http://127.0.0.1:8931/mcp".into(),
            jwks_uri: "file:///r/.branchyard/gateway/jwks.json".into(),
            audit_file: "/r/.branchyard/gateway/audit.jsonl".into(),
            vault_key_file: "/r/.branchyard/gateway/vault.key".into(),
            vault_dir: None,
            public_url: None,
        }
    }

    #[test]
    fn the_command_is_anvils_fleet_in_branchyard_mode() {
        let c = command(&["node", "/opt/anvil/bin-anvil.js"]);
        assert_eq!(
            c.args(),
            [
                "/opt/anvil/bin-anvil.js",
                "serve",
                "mcp",
                "/srv/bundles",
                "--fleet",
                "--http",
                "8931"
            ]
        );
        let env: std::collections::BTreeMap<_, _> = c.env().into_iter().collect();
        assert_eq!(env["ANVIL_INBOUND_AUTH_MODE"], "branchyard");
        assert_eq!(env["ANVIL_INBOUND_AUDIENCE"], "http://127.0.0.1:8931/mcp");
        assert_eq!(
            env["ANVIL_INBOUND_JWKS_URI"],
            "file:///r/.branchyard/gateway/jwks.json"
        );
        assert_eq!(
            env["ANVIL_VAULT_KEY_FILE"],
            "/r/.branchyard/gateway/vault.key"
        );
    }

    fn initialize_result() -> serde_json::Value {
        json!({"jsonrpc": "2.0", "id": 0, "result": {
            "protocolVersion": mcp::PROTOCOL_VERSION,
            "capabilities": {}, "serverInfo": {"name": "anvil", "version": "1"}
        }})
    }

    /// A port that answers an MCP `initialize` — in JSON, as an event
    /// stream, or by refusing it with a bearer challenge — is a gateway.
    #[test]
    fn a_port_that_speaks_mcp_is_a_gateway() {
        let json = MockHttp::start(|_| Response::json(200, &initialize_result()));
        let url = format!("{}/mcp", json.url());
        assert_eq!(probe(&url), Held::Gateway);
        let request = json.await_requests(1).remove(0);
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/mcp");
        assert_eq!(request.json()["method"], "initialize");
        assert!(
            request.header("authorization").is_none(),
            "no token is sent"
        );

        let stream = MockHttp::start(|_| {
            Response::new(
                200,
                format!("event: message\ndata: {}\n\n", initialize_result()),
            )
            .header("Content-Type", "text/event-stream")
        });
        assert_eq!(probe(&format!("{}/mcp", stream.url())), Held::Gateway);

        let guarded = MockHttp::start(|_| {
            Response::new(401, "").header(
                "WWW-Authenticate",
                "Bearer resource_metadata=\"/.well-known/x\"",
            )
        });
        assert_eq!(probe(&format!("{}/mcp", guarded.url())), Held::Gateway);

        let refusing = MockHttp::start(|_| {
            Response::json(
                401,
                &json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32001, "message": "no token"}}),
            )
        });
        assert_eq!(probe(&format!("{}/mcp", refusing.url())), Held::Gateway);
    }

    /// What is not a gateway: an HTTP server that does not speak MCP, a
    /// listener that is not HTTP, and one that accepts and says nothing.
    #[test]
    fn a_port_that_does_not_speak_mcp_is_not_a_gateway() {
        let web = MockHttp::start(|_| Response::new(404, "<html>not found</html>"));
        let held = probe(&format!("{}/mcp", web.url()));
        assert!(
            matches!(&held, Held::Other(why) if why.contains("404")),
            "{held:?}"
        );
        let page = MockHttp::start(|_| Response::new(200, "<html>hello</html>"));
        let held = probe(&format!("{}/mcp", page.url()));
        assert!(
            matches!(&held, Held::Other(why) if why.contains("not with an MCP initialize result")),
            "{held:?}"
        );
        // Another JSON-RPC server (a blockchain node's) is not an MCP one.
        let node = MockHttp::start(|_| {
            Response::json(
                200,
                &json!({"jsonrpc": "2.0", "id": 0, "error": {"code": -32601, "message": "Method not found"}}),
            )
        });
        assert!(matches!(
            probe(&format!("{}/mcp", node.url())),
            Held::Other(_)
        ));
        let plain_auth = MockHttp::start(|_| Response::new(401, "").header("X-Other", "1"));
        assert!(matches!(
            probe(&format!("{}/mcp", plain_auth.url())),
            Held::Other(_)
        ));

        // Not HTTP at all: a banner, then the connection closes.
        let raw = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/mcp", raw.local_addr().unwrap());
        let banner = std::thread::spawn(move || {
            // The probe's first connection only checks that it connects.
            for stream in raw.incoming().take(2) {
                let mut stream = stream.unwrap();
                branchyard_support::best_effort(
                    "write a banner",
                    stream.write_all(b"SSH-2.0-OpenSSH_9.6\r\n"),
                );
            }
        });
        assert!(matches!(probe(&url), Held::Other(_)));
        banner.join().unwrap();

        // Accepts and never speaks: not waited on for long.
        let silent = MockHttp::start(|_| Response::hang());
        let held = probe_within(&format!("{}/mcp", silent.url()), Duration::from_millis(300));
        assert!(matches!(held, Held::Other(_)), "{held:?}");
        silent.finish();
    }

    #[test]
    fn a_port_nobody_listens_on_holds_nothing() {
        // Port 1 (tcpmux) has no listener on any machine this runs on.
        assert_eq!(probe("http://127.0.0.1:1/mcp"), Held::Nothing);
        assert_eq!(probe("not a url"), Held::Nothing);
    }

    #[test]
    fn urls_give_their_host_and_port() {
        assert_eq!(
            host_port("http://127.0.0.1:8931/mcp"),
            Some(("127.0.0.1".into(), 8931))
        );
        assert_eq!(
            host_port("https://gw.example/mcp"),
            Some(("gw.example".into(), 443))
        );
        assert_eq!(host_port("http://[::1]:9/mcp"), Some(("::1".into(), 9)));
        assert_eq!(host_port("ftp://x"), None);
    }

    #[test]
    fn the_vault_key_is_made_once_and_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("gw/vault.key");
        ensure_vault_key(&key).unwrap();
        let first = fs::read_to_string(&key).unwrap();
        assert_eq!(first.len(), 64);
        assert!(first.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(
            fs::metadata(&key).unwrap().permissions().mode() & 0o777,
            0o600
        );
        ensure_vault_key(&key).unwrap();
        assert_eq!(fs::read_to_string(&key).unwrap(), first);
    }

    #[test]
    fn a_supervisor_restarts_an_exiting_gateway_and_stops_it() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("gateway.log");
        let marker = dir.path().join("starts");
        // A "gateway" that records each start and exits at once.
        let script = format!("echo start >> {}; exit 3", marker.display());
        let c = command(&["sh", "-c", &script, "anvil"]);
        let ticks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = ticks.clone();
        let supervisor = Supervisor::start(
            c,
            log.clone(),
            Box::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
            }),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while fs::read_to_string(&marker)
            .unwrap_or_default()
            .lines()
            .count()
            < 2
        {
            assert!(Instant::now() < deadline, "not restarted");
            std::thread::sleep(Duration::from_millis(50));
        }
        drop(supervisor);
        assert!(ticks.load(Ordering::SeqCst) >= 2);
        let text = fs::read_to_string(&log).unwrap();
        assert!(text.contains("restarting"), "{text}");
    }
}
