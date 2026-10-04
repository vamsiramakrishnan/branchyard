//! In-process servers over real HTTP on 127.0.0.1, temporary repositories,
//! and the fake ACP agent. Hermetic: no real harness and no network beyond
//! loopback. Requires `git` and `sh`.

#![allow(dead_code)]

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Once;
use std::time::Duration;

use branchyard_client::api::{Operation, PolicySpec, TaskRequest};
use branchyard_client::{new_key, Client};
use branchyard_server::config::{Principal, TenantPolicy, Token};
use branchyard_server::{Config, Running, Stopped};
use branchyard_testkit::wait;

pub use branchyard_testkit::fake_agent_here as fake_agent;

pub const TOKEN: &str = "test-token-0123456789";

static COUNTER: AtomicU64 = AtomicU64::new(0);
static HERMETIC: Once = Once::new();

pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// A temporary directory with a repository at `repo/` (one commit on
/// `main`) and a server data directory at `data/`. Removed on drop.
pub struct Fixture {
    pub dir: PathBuf,
    pub root: PathBuf,
    pub data: PathBuf,
}

impl Fixture {
    pub fn new() -> Fixture {
        HERMETIC.call_once(|| {
            std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
            std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        });
        // Build the agent before any test body starts timing.
        fake_agent();
        let dir = std::env::temp_dir().join(format!(
            "branchyard-server-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("repo")).unwrap();
        let dir = fs::canonicalize(dir).unwrap();
        let root = dir.join("repo");
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "Test"]);
        git(&root, &["config", "user.email", "test@localhost"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        fs::write(root.join("a.txt"), "one\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "initial"]);
        Fixture {
            data: dir.join("data"),
            dir,
            root,
        }
    }

    /// A second repository under this fixture's directory, with one commit
    /// on `main`, for tests of more than one served repository (such as
    /// per-tenant repository ownership). Not served unless added to a
    /// `Config`'s `repos`.
    pub fn extra_repo(&self, name: &str) -> PathBuf {
        let root = self.dir.join(name);
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "Test"]);
        git(&root, &["config", "user.email", "test@localhost"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        fs::write(root.join("a.txt"), "one\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "initial"]);
        root
    }

    /// A configuration serving the repository as `app` on an ephemeral
    /// loopback port, with the fake agent as `gemini-cli`.
    pub fn config(&self) -> Config {
        let mut config = Config::new(self.data.clone());
        config.listen = "127.0.0.1:0".parse().unwrap();
        config.repos = vec![("app".into(), self.root.clone())];
        config.tokens = vec![Token {
            name: "tester".into(),
            secret: TOKEN.into(),
        }];
        config.harness_commands.insert(
            "gemini-cli".into(),
            vec![fake_agent().display().to_string()],
        );
        config.shutdown_grace = Duration::from_secs(10);
        config.poll_interval = Duration::from_millis(100);
        config.log_requests = false;
        // Never this machine's real harnesses: a worker advertises none
        // unless a test gives it an inventory (tests/inventory.rs).
        config.inventory_source = Some(branchyard_server::ops::InventorySource(
            std::sync::Arc::new(|| None),
        ));
        config
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// A server on its own runtime. Shut down on drop.
pub struct Server {
    pub runtime: tokio::runtime::Runtime,
    pub running: Option<Running>,
    pub addr: SocketAddr,
}

impl Server {
    pub fn start(config: Config) -> Server {
        Server::try_start(config).unwrap()
    }

    pub fn try_start(config: Config) -> Result<Server, String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let running = runtime.block_on(branchyard_server::start(config))?;
        let addr = running.local_addr();
        Ok(Server {
            runtime,
            running: Some(running),
            addr,
        })
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn client(&self) -> Client {
        Client::new(&self.url(), TOKEN).unwrap()
    }

    /// Shut down and wait, as a signal would.
    pub fn stop(mut self) -> Stopped {
        self.stop_inner()
    }

    fn stop_inner(&mut self) -> Stopped {
        let running = self.running.take().expect("running");
        running.shutdown();
        self.runtime.block_on(running.wait())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if self.running.is_some() {
            self.stop_inner();
        }
    }
}

pub const ACME_TOKEN: &str = "acme-token-0123456789ab";
pub const GLOBEX_TOKEN: &str = "globex-token-0123456789";

/// Two tenants on `config`: `acme` owning `app` and `globex` owning the
/// repository at `appb` (served as `appb`), each with one principal of its
/// own name holding every scope ([`ACME_TOKEN`], [`GLOBEX_TOKEN`]), and
/// `tenant_max_running` as both tenants' `max_running`.
pub fn two_tenants(config: &mut Config, appb: PathBuf, tenant_max_running: Option<usize>) {
    config.repos.push(("appb".into(), appb));
    config.tokens = Vec::new();
    for (tenant, secret, repo) in [
        ("acme", ACME_TOKEN, "app"),
        ("globex", GLOBEX_TOKEN, "appb"),
    ] {
        config.tokens.push(Token {
            name: tenant.into(),
            secret: secret.into(),
        });
        let mut principal = Principal::default_for(tenant);
        principal.tenant = tenant.into();
        config.principals.insert(tenant.into(), principal);
        config.tenants.insert(
            tenant.into(),
            TenantPolicy {
                repos: Some([repo.to_owned()].into_iter().collect()),
                max_running: tenant_max_running,
                ..TenantPolicy::default()
            },
        );
    }
}

/// Whether `branch` of `repo` exists and its harness has been sent a
/// prompt: its turn has started.
pub fn started(client: &Client, repo: &str, branch: &str) -> bool {
    client.repo(repo).events(branch, 0).is_ok_and(|page| {
        page.events
            .iter()
            .any(|e| matches!(e.activity, branchyard::Activity::Prompt(_)))
    })
}

/// A task for the fake agent, allowed every permission.
pub fn task(prompt: &str, name: &str) -> TaskRequest {
    TaskRequest {
        prompt: prompt.into(),
        harness: Some("gemini-cli".into()),
        name: Some(name.into()),
        policy: PolicySpec::allow_all(),
        ..TaskRequest::default()
    }
}

/// Submit and wait for the result.
pub fn run(client: &Client, request: &TaskRequest) -> Operation {
    let op = client.repo("app").submit_task(request, &new_key()).unwrap();
    await_operation(client, &op.id)
}

/// A raw HTTP/1.1 exchange: status and body.
pub fn raw(addr: SocketAddr, request: &str) -> (u16, String, String) {
    raw_bytes(addr, request.as_bytes())
}

pub fn raw_bytes(addr: SocketAddr, request: &[u8]) -> (u16, String, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    // The server may answer and close before reading everything.
    let _ = stream.write_all(request);
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    let text = String::from_utf8_lossy(&response).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, head.to_owned(), body.to_owned())
}

/// A JSON `POST` as raw bytes, with optional extra header lines.
pub fn post(path: &str, token: Option<&str>, extra: &str, body: &str) -> String {
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    format!(
        "POST {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n{auth}{extra}\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

pub fn get(path: &str, token: Option<&str>) -> String {
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n{auth}\r\n")
}

/// Wait for operation `id` to reach a terminal state; returns it.
pub fn await_operation(client: &Client, id: &str) -> Operation {
    wait::until(&format!("operation {id} to finish"), || {
        let op = client.operation(id).unwrap();
        if op.state.is_terminal() {
            Ok(op)
        } else {
            Err(format!("{:?}", op.state))
        }
    })
}
