//! `by gateway start|stop|status|rotate-key|jwks` and `by connect`: the
//! repository's connector gateway, Anvil's, run and supervised here, and
//! the configuration that gives the yard its gateway. See
//! docs/connectors.md.
//!
//! `[connectors]` in branchyard.toml names the gateway's `/mcp` URL, the
//! bundle root and Anvil's command. Locally the yard signs with
//! `.branchyard/gateway/key`, publishes `jwks.json` beside it for the
//! gateway (a `file:` URL), and reads the gateway's audit log from
//! `audit.jsonl` there.

use branchyard_support::best_effort;
use branchyard_support::time::now_ms;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use branchyard::connectors::gateway::{self, Background, GatewayCommand, Supervisor};
use branchyard::connectors::{self, AnvilPackager, Gateway, KeyRing};
use branchyard::services::{
    Endpoint, Health, Query, Reclaim, Service, ServiceOwner, ServiceStore, KIND_CONNECTOR_GATEWAY,
};
use branchyard::Yard;
use branchyard_setup::config::{split_words, Connectors};
use serde_json::json;

use crate::args::GatewayAction;
use crate::commands::{self, print, Failure, Outcome, Target};
use crate::{json, setup_io};

/// The default command for Anvil.
const ANVIL: &str = "anvil";

/// The variable a background `by gateway start` gives the supervisor it
/// starts: the URL it chose, when none is pinned.
const ENV_URL: &str = "BRANCHYARD_GATEWAY_URL";

/// `[connectors]` as the yard at `root` has it, perhaps without a gateway
/// URL (the default when nothing is configured); `None` inside a harness
/// on a branch (whose `by` acts through its engine).
fn settings(root: &Path) -> Result<Option<Connectors>, Failure> {
    if std::env::var_os("BRANCHYARD_BRANCH").is_some_and(|v| !v.is_empty()) {
        return Ok(None);
    }
    let located = setup_io::locate(root);
    if !located.project_exists && !located.user.is_file() {
        return Ok(Some(Connectors::default()));
    }
    let effective = setup_io::load(root, None).map_err(|e| {
        Failure::Message(format!(
            "{e}\n(fix it, or check it with `by config validate`)"
        ))
    })?;
    Ok(Some(effective.config.connectors))
}

/// Where the yard's gateway is: `[connectors] gateway`, an explicit pin,
/// or else the live gateway registered for this yard (its issuer) in the
/// repository's service registry. See docs/registry.md.
enum Found {
    Pinned(String),
    Registered(Box<Service>),
}

impl Found {
    fn url(&self) -> &str {
        match self {
            Found::Pinned(url) => url,
            Found::Registered(service) => service.url().unwrap_or_default(),
        }
    }

    fn source(&self) -> &'static str {
        match self {
            Found::Pinned(_) => "pinned",
            Found::Registered(_) => "registry",
        }
    }
}

fn find(yard: &Yard, config: &Connectors) -> Result<Option<Found>, Failure> {
    if let Some(url) = &config.gateway {
        return Ok(Some(Found::Pinned(url.clone())));
    }
    if !yard.has_services() {
        return Ok(None);
    }
    let issuer = connectors::local_issuer(yard.root())?;
    let query = Query::kind(KIND_CONNECTOR_GATEWAY).require("issuer", issuer);
    Ok(yard
        .resolve_service(&query)?
        .filter(|s| s.url().is_some())
        .map(|s| Found::Registered(Box::new(s))))
}

fn anvil_command(config: &Connectors) -> Result<Vec<String>, Failure> {
    let words = split_words(config.anvil.as_deref().unwrap_or(ANVIL))
        .map_err(|e| Failure::Message(format!("connectors.anvil: {e}")))?;
    match words.is_empty() {
        true => Err(Failure::Message("connectors.anvil names no command".into())),
        false => Ok(words),
    }
}

fn bundles(root: &Path, config: &Connectors) -> PathBuf {
    config
        .bundles
        .as_ref()
        .map_or_else(|| root.join("connectors"), PathBuf::from)
}

fn local_gateway(yard: &Yard, config: &Connectors, found: &Found) -> Result<Gateway, Failure> {
    let packager = AnvilPackager {
        command: anvil_command(config)?,
        root: bundles(yard.root(), config),
    };
    let mut gateway = Gateway::local(yard, found.url(), Arc::new(packager))?;
    // Each turn's effect-ledger proxy (docs/effects.md).
    if let Some(enabled) = config.effects_proxy {
        gateway.effects.enabled = enabled;
    }
    if let Some(listen) = config.effects_listen.as_deref() {
        gateway.effects.listen = listen
            .parse()
            .map_err(|_| Failure::Message(format!("connectors.effects_listen: {listen:?}")))?;
    }
    gateway.effects.sandbox_host = config.effects_sandbox_host.clone();
    gateway.sandbox_url = config.sandbox_gateway.clone().or_else(|| match found {
        Found::Registered(service) => service.text("sandbox_url").map(str::to_owned),
        Found::Pinned(_) => None,
    });
    Ok(gateway)
}

/// The gateway the yard at `root` has, pinned or registered, if any, for
/// a call `by` makes as you (`--issue` through a tracker's connector; see
/// `crate::trackers`).
pub fn configured(yard: &Yard) -> Result<Option<Gateway>, Failure> {
    let Some(config) = settings(yard.root())? else {
        return Ok(None);
    };
    match find(yard, &config)? {
        Some(found) => Ok(Some(local_gateway(yard, &config, &found)?)),
        None => Ok(None),
    }
}

/// Give `yard` its gateway, pinned or registered, if it has one.
pub fn configure(yard: &Yard) -> Result<(), Failure> {
    if let Some(gateway) = configured(yard)? {
        yard.use_connectors(gateway);
    }
    Ok(())
}

fn local_only(target: &Target, what: &str) -> Result<(), Failure> {
    match target {
        Target::Local => Ok(()),
        Target::Remote(_) => Err(Failure::Message(format!(
            "{what} runs here; a server runs its own gateway beside `by serve` (docs/server.md)"
        ))),
    }
}

fn inside_harness() -> Failure {
    Failure::Message("a harness on a branch reaches the gateway through its turn's token".into())
}

fn require(yard: &Yard) -> Result<(Connectors, Gateway, Found), Failure> {
    let config = settings(yard.root())?.ok_or_else(inside_harness)?;
    let found = find(yard, &config)?.ok_or_else(|| {
        Failure::Message(
            "no connector gateway runs for this repository and none is pinned; `by gateway \
             start` starts one, or set [connectors] gateway in branchyard.toml \
             (docs/connectors.md)"
                .into(),
        )
    })?;
    let gateway = local_gateway(yard, &config, &found)?;
    Ok((config, gateway, found))
}

/// A URL on a free loopback port, for a gateway nothing pins.
fn free_url() -> Result<String, Failure> {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map_err(|e| Failure::Message(format!("could not find a free port: {e}")))?
        .port();
    Ok(format!("http://127.0.0.1:{port}/mcp"))
}

/// The gateway process for this yard, its keys made first.
fn gateway_command(
    yard: &Yard,
    config: &Connectors,
    gw: &Gateway,
) -> Result<GatewayCommand, Failure> {
    let dir = connectors::local_dir(yard.root());
    gw.keys()?;
    let vault_key = config
        .vault_key
        .as_ref()
        .map_or_else(|| dir.join("vault.key"), PathBuf::from);
    gateway::ensure_vault_key(&vault_key)?;
    let (host, port) = gateway::host_port(&gw.url).ok_or_else(|| {
        Failure::Message(format!(
            "connectors.gateway {} has no host and port",
            gw.url
        ))
    })?;
    let jwks = gw
        .jwks_file
        .clone()
        .unwrap_or_else(|| dir.join("jwks.json"));
    Ok(GatewayCommand {
        anvil: anvil_command(config)?,
        bundles: bundles(yard.root(), config),
        port,
        host: config.listen.clone().or_else(|| {
            (!matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1")).then_some(host)
        }),
        issuer: gw.issuer.clone(),
        audience: gw.url.clone(),
        jwks_uri: format!("file://{}", jwks.display()),
        audit_file: gw
            .audit_file
            .clone()
            .unwrap_or_else(|| dir.join("audit.jsonl")),
        vault_key_file: vault_key,
        vault_dir: Some(dir.join("vault")),
        public_url: None,
    })
}

/// How often the gateway's supervisor reconciles the effect ledger.
const RECONCILE_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

pub fn main(target: &Target, action: &GatewayAction, as_json: bool) -> Outcome {
    local_only(target, "by gateway")?;
    let yard = commands::open()?;
    let dir = connectors::local_dir(yard.root());
    match action {
        GatewayAction::Start { foreground } => start(&yard, &dir, *foreground, as_json),
        GatewayAction::Stop => match Background::running(&dir) {
            Some(running) => {
                running.stop(&dir)?;
                // What it wrote before it stopped, and its record (a
                // supervisor stopped by a signal leaves it live).
                best_effort(
                    "ingest the connector audit log",
                    yard.ingest_connector_audit(),
                );
                best_effort(
                    "reclaim the services of stopped processes",
                    yard.reclaim_services(),
                );
                say(
                    as_json,
                    json!({"stopped": true, "pid": running.pid}),
                    &format!("stopped the gateway (pid {})\n", running.pid),
                )
            }
            None => say(
                as_json,
                json!({"stopped": false}),
                "no gateway is running here\n",
            ),
        },
        GatewayAction::Status => status(&yard, &dir, as_json),
        GatewayAction::RotateKey { keep } => {
            let (_, gw, _) = require(&yard)?;
            let mut ring = gw.keys()?;
            let kid = ring.rotate(*keep)?;
            ring.save(&gw.key_file, gw.jwks_file.as_deref())?;
            say(
                as_json,
                json!({"kid": kid, "kids": ring.kids()}),
                &format!(
                    "signing with {kid}; publishing {}\n",
                    ring.kids().join(", ")
                ),
            )
        }
        GatewayAction::Jwks => {
            let (_, gw, _) = require(&yard)?;
            print(&json::text(&gw.jwks()?))
        }
    }
}

fn say(as_json: bool, value: serde_json::Value, text: &str) -> Outcome {
    match as_json {
        true => print(&json::text(&value)),
        false => print(text),
    }
}

fn start(yard: &Yard, dir: &Path, foreground: bool, as_json: bool) -> Outcome {
    let config = settings(yard.root())?.ok_or_else(inside_harness)?;
    if let Some(running) = Background::running(dir) {
        return say(
            as_json,
            json!({"started": false, "pid": running.pid, "url": running.url}),
            &format!(
                "the gateway is already running (pid {}) at {}\n",
                running.pid, running.url
            ),
        );
    }
    // The URL: the one a background start chose for this supervisor, the
    // pinned one, or a free loopback port.
    let chosen = std::env::var(ENV_URL).ok().filter(|u| !u.is_empty());
    let found = match chosen {
        Some(url) => Found::Pinned(url),
        None => match find(yard, &config)? {
            Some(Found::Registered(service)) => {
                let url = service.url().unwrap_or_default().to_owned();
                if gateway::listening(&url) {
                    return say(
                        as_json,
                        json!({"started": false, "url": url, "service": service.id}),
                        &format!("a gateway for this repository already runs at {url}\n"),
                    );
                }
                Found::Pinned(free_url()?)
            }
            Some(found) => found,
            None => Found::Pinned(free_url()?),
        },
    };
    let gw = local_gateway(yard, &config, &found)?;
    // Something already listens at the pinned URL, not started here:
    // adopt it, so that it is found by what it is, but never reclaim it.
    if config.gateway.is_some() && !foreground && gateway::listening(&gw.url) {
        let service = adopt(yard, &config, &gw)?;
        return say(
            as_json,
            json!({"started": false, "adopted": true, "url": gw.url, "service": service.id}),
            &format!(
                "a gateway this repository did not start listens at {}; adopted it as {} \
                 (never stopped by Branchyard)\n",
                gw.url, service.id
            ),
        );
    }
    let command = gateway_command(yard, &config, &gw)?;
    let log = dir.join("gateway.log");
    if foreground {
        return supervise(yard, &config, &gw, command, &log);
    }
    // In the background: this same command, in the foreground, in a
    // process group of its own that `by gateway stop` ends.
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe()?;
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)?;
    Command::new(exe)
        .args(["gateway", "start", "--foreground"])
        .current_dir(yard.root())
        .env(ENV_URL, &gw.url)
        .stdin(Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out)
        .process_group(0)
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut started = None;
    while Instant::now() < deadline {
        started = started.or_else(|| Background::running(dir));
        if started.is_some() && gateway::listening(&gw.url) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let Some(started) = started else {
        return Err(Failure::Message(format!(
            "the gateway's supervisor did not start; see {}",
            log.display()
        )));
    };
    let listening = gateway::listening(&gw.url);
    say(
        as_json,
        json!({"started": true, "pid": started.pid, "url": gw.url, "listening": listening,
               "log": log, "source": found.source()}),
        &match listening {
            true => format!(
                "the gateway runs at {} (pid {}); log {}\n",
                gw.url,
                started.pid,
                log.display()
            ),
            false => format!(
                "the gateway's supervisor runs (pid {}) but nothing listens at {} yet; see {}\n",
                started.pid,
                gw.url,
                log.display()
            ),
        },
    )
}

/// How long an adopted gateway's record lasts: nothing renews it but the
/// next `by gateway start` or `status` that finds it listening.
const ADOPTED_TTL: Duration = Duration::from_secs(600);

/// Register the gateway listening at `gw.url` that this repository did not
/// start: owned by no process (so never thought gone), with nothing to
/// reclaim, for [`ADOPTED_TTL`].
fn adopt(yard: &Yard, config: &Connectors, gw: &Gateway) -> Result<Service, Failure> {
    let id = format!(
        "connector_gateway-adopted-{}",
        gateway::host_port(&gw.url).map_or(0, |(_, port)| port)
    );
    let mut service = describe(
        yard,
        config,
        gw,
        ServiceOwner::remote(id.clone(), "adopted"),
    )
    .with_id(id)
    .with("adopted", true);
    service.owner.principal = None;
    let now = now_ms();
    service.lease_until_ms = now + ADOPTED_TTL.as_millis() as u64;
    Ok(yard.services()?.register(&service, now)?)
}

/// The gateway at `gw.url` as a service: what it serves, for whom, and
/// where.
fn describe(yard: &Yard, config: &Connectors, gw: &Gateway, owner: ServiceOwner) -> Service {
    let served: Vec<String> = connectors::packager::discover(&bundles(yard.root(), config))
        .map(|b| b.into_iter().map(|b| b.id).collect())
        .unwrap_or_default();
    let mut service = Service::new(KIND_CONNECTOR_GATEWAY, owner)
        .with("issuer", gw.issuer.clone())
        .with("connectors", served)
        .with("protocol", "mcp-streamable-http")
        .with("root", yard.root().display().to_string())
        .with_endpoint(Endpoint::url(gw.url.clone()));
    if let Some(url) = &gw.sandbox_url {
        service = service.with("sandbox_url", url.clone());
    }
    service
}

/// The gateway process `pid` as what to reclaim: with this supervisor's
/// group when it leads one (started in the background), so what is left
/// of the group goes too.
fn reclaim_of(pid: Option<u32>) -> Option<Reclaim> {
    match Reclaim::process(pid?, false)? {
        Reclaim::Process {
            host, pid, start, ..
        } => Some(Reclaim::Process {
            host,
            pid,
            start,
            group: Reclaim::own_group(),
        }),
        other => Some(other),
    }
}

/// `by gateway start --foreground`: supervise the gateway in this process
/// until interrupted, registered in the repository's service registry
/// with the gateway process to reclaim should this process stop without
/// stopping it.
fn supervise(
    yard: &Yard,
    config: &Connectors,
    gw: &Gateway,
    command: GatewayCommand,
    log: &Path,
) -> Outcome {
    let dir = connectors::local_dir(yard.root());
    Background::this_process(&gw.url, log).save(&dir)?;
    // A record a stopped supervisor left, and its gateway with it.
    best_effort(
        "reclaim the services of stopped processes",
        yard.reclaim_services(),
    );
    let reader = yard.clone();
    // The effect ledger is reconciled on this timer too (docs/effects.md):
    // unknown outcomes looked up through the gateway it supervises.
    let reconciled = std::sync::atomic::AtomicU64::new(0);
    let supervisor = Supervisor::start(
        command,
        log.to_path_buf(),
        Box::new(move || {
            branchyard_support::best_effort_once(
                "ingest the connector audit log",
                reader.ingest_connector_audit(),
            );
            let now = branchyard_support::time::now_ms() / 1000;
            let last = reconciled.load(std::sync::atomic::Ordering::Relaxed);
            if now.saturating_sub(last) >= RECONCILE_EVERY.as_secs() {
                reconciled.store(now, std::sync::atomic::Ordering::Relaxed);
                branchyard_support::best_effort_once(
                    "reconcile the effect ledger",
                    reader.reconcile_effects(),
                );
            }
        }),
    )?;
    eprintln!(
        "by: the gateway serves {} at {}; its log is {} (interrupt to stop)",
        bundles(yard.root(), config).display(),
        gw.url,
        log.display()
    );
    let registration = yard.register_service(
        describe(yard, config, gw, ServiceOwner::this_process()).with_health(Health::Starting),
        branchyard::services::DEFAULT_TTL,
    );
    let registration = match registration {
        Ok(registration) => Some(registration),
        Err(e) => {
            eprintln!("by: could not register the gateway in the service registry: {e}");
            None
        }
    };
    let mut reclaimed: Option<u32> = None;
    let mut health = Health::Starting;
    loop {
        if let Some(registration) = &registration {
            let pid = supervisor.pid();
            if pid != reclaimed {
                let reclaim = reclaim_of(pid);
                if registration.update(|s| s.reclaim = reclaim.clone()).is_ok() {
                    reclaimed = pid;
                }
            }
            let now = match pid.is_some() && gateway::listening(&gw.url) {
                true => Health::Healthy,
                false => Health::Starting,
            };
            if now != health && registration.set_health(now).is_ok() {
                health = now;
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn status(yard: &Yard, dir: &Path, as_json: bool) -> Outcome {
    let (config, gw, found) = require(yard)?;
    let running = Background::running(dir);
    let listening = gateway::listening(&gw.url);
    let service = match &found {
        Found::Registered(service) => Some(service.id.clone()),
        Found::Pinned(_) => None,
    };
    let root = bundles(yard.root(), &config);
    let served = connectors::packager::discover(&root);
    let kids = KeyRing::load(&gw.key_file)
        .map(|k| k.kids())
        .unwrap_or_default();
    if as_json {
        return print(&json::text(&json!({
            "url": gw.url,
            "source": found.source(),
            "service": service,
            "sandbox_url": gw.sandbox_url,
            "issuer": gw.issuer,
            "running": running.as_ref().map(|r| json!({"pid": r.pid, "log": r.log})),
            "listening": listening,
            "bundles": root,
            "connectors": served.as_ref().map(|b| b.iter().map(|b| b.id.clone()).collect::<Vec<_>>()).unwrap_or_default(),
            "bundles_error": served.as_ref().err(),
            "keys": kids,
            "audit_file": gw.audit_file,
        })));
    }
    let mut out = format!("gateway   {} ({})\n", gw.url, found.source());
    out.push_str(&match (&running, listening) {
        (Some(r), true) => format!(
            "running   pid {}, listening; log {}\n",
            r.pid,
            r.log.display()
        ),
        (Some(r), false) => format!(
            "running   pid {}, not listening yet; log {}\n",
            r.pid,
            r.log.display()
        ),
        (None, true) => "running   elsewhere (something listens there; not started here)\n".into(),
        (None, false) => "stopped   (by gateway start)\n".into(),
    });
    if let Some(url) = &gw.sandbox_url {
        out.push_str(&format!("sandboxes {url}\n"));
    }
    out.push_str(&format!("issuer    {}\n", gw.issuer));
    out.push_str(&match &served {
        Ok(bundles) if bundles.is_empty() => {
            format!("serves    nothing under {}\n", root.display())
        }
        Ok(bundles) => format!(
            "serves    {} (from {})\n",
            bundles
                .iter()
                .map(|b| b.id.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            root.display()
        ),
        Err(e) => format!("serves    ? ({e})\n"),
    });
    out.push_str(&match kids.is_empty() {
        true => "keys      none yet (made on the first token or start)\n".into(),
        false => format!("keys      {} (signing with the first)\n", kids.join(", ")),
    });
    print(&out)
}

/// `by connect <connector> [--account NAME] [--api-key-stdin] [--open]`:
/// `anvil connect <bundles> <connector>` as the person, with their connect
/// token (`by_purpose: "connect"`, ten minutes, no grant), the only token the
/// gateway's connect routes take. The gateway answers with an
/// authorization URL, or takes a key from stdin.
pub fn connect(
    target: &Target,
    connector: &str,
    account: Option<&str>,
    api_key_stdin: bool,
    open: bool,
) -> Outcome {
    local_only(target, "by connect")?;
    connectors::check_connector(connector).map_err(Failure::Message)?;
    let yard = commands::open()?;
    let (config, gw, _) = require(&yard)?;
    let token = gw.connect_token(None, connectors::CONNECT_TTL)?;
    let dir = connectors::local_dir(yard.root());
    let file = dir.join(format!("connect-{}.token", std::process::id()));
    write_private(&file, token.as_bytes())?;
    let anvil = anvil_command(&config)?;
    let mut command = Command::new(&anvil[0]);
    command
        .args(&anvil[1..])
        .arg("connect")
        .arg(bundles(yard.root(), &config))
        .arg(connector)
        .env(connectors::ENV_GATEWAY_URL, &gw.url)
        .env(connectors::ENV_GATEWAY_TOKEN_FILE, &file);
    if let Some(account) = account {
        command.args(["--account", account]);
    }
    if api_key_stdin {
        command.arg("--api-key-stdin");
    }
    if open {
        command.arg("--open");
    }
    let status = command.status();
    branchyard_support::cleanup_file(&file);
    let status =
        status.map_err(|e| Failure::Message(format!("could not run {}: {e}", anvil[0])))?;
    match status.success() {
        true => Ok(()),
        false => Err(Failure::Message(format!(
            "anvil connect {connector} failed ({status})"
        ))),
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), Failure> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    Ok(())
}
