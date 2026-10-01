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

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use branchyard::connectors::gateway::{self, Background, GatewayCommand, Supervisor};
use branchyard::connectors::{self, AnvilPackager, Gateway, KeyRing};
use branchyard::Yard;
use branchyard_setup::config::{split_words, Connectors};
use serde_json::json;

use crate::args::GatewayAction;
use crate::commands::{self, print, Failure, Outcome, Target};
use crate::{json, setup_io};

/// The default command for Anvil.
const ANVIL: &str = "anvil";

/// `[connectors]` as the yard at `root` uses it: `None` without a gateway,
/// or inside a harness on a branch (whose `by` acts through its engine).
fn config(root: &Path) -> Result<Option<Connectors>, Failure> {
    if std::env::var_os("BRANCHYARD_BRANCH").is_some_and(|v| !v.is_empty()) {
        return Ok(None);
    }
    let located = setup_io::locate(root);
    if !located.project_exists && !located.user.is_file() {
        return Ok(None);
    }
    let effective = setup_io::load(root, None).map_err(|e| {
        Failure::Message(format!(
            "{e}\n(fix it, or check it with `by config validate`)"
        ))
    })?;
    let connectors = effective.config.connectors;
    Ok(connectors.gateway.is_some().then_some(connectors))
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

fn local_gateway(yard: &Yard, config: &Connectors) -> Result<Gateway, Failure> {
    let url = config.gateway.clone().unwrap_or_default();
    let packager = AnvilPackager {
        command: anvil_command(config)?,
        root: bundles(yard.root(), config),
    };
    let mut gateway = Gateway::local(yard, &url, Arc::new(packager))?;
    gateway.sandbox_url = config.sandbox_gateway.clone();
    Ok(gateway)
}

/// Give `yard` the gateway `[connectors]` configures, if any.
pub fn configure(yard: &Yard) -> Result<(), Failure> {
    if let Some(config) = config(yard.root())? {
        yard.use_connectors(local_gateway(yard, &config)?);
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

fn require(yard: &Yard) -> Result<(Connectors, Gateway), Failure> {
    let config = config(yard.root())?.ok_or_else(|| {
        Failure::Message(
            "no connector gateway is configured; set [connectors] gateway (and bundles, anvil) \
             in branchyard.toml (docs/connectors.md)"
                .into(),
        )
    })?;
    let gateway = local_gateway(yard, &config)?;
    Ok((config, gateway))
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

pub fn main(target: &Target, action: &GatewayAction, as_json: bool) -> Outcome {
    local_only(target, "by gateway")?;
    let yard = commands::open()?;
    let dir = connectors::local_dir(yard.root());
    match action {
        GatewayAction::Start { foreground } => start(&yard, &dir, *foreground, as_json),
        GatewayAction::Stop => match Background::running(&dir) {
            Some(running) => {
                running.stop(&dir)?;
                // What it wrote before it stopped.
                let _ = yard.ingest_connector_audit();
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
            let (_, gw) = require(&yard)?;
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
            let (_, gw) = require(&yard)?;
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
    let (config, gw) = require(yard)?;
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
    let command = gateway_command(yard, &config, &gw)?;
    let log = dir.join("gateway.log");
    if foreground {
        Background::this_process(&gw.url, &log).save(dir)?;
        let reader = yard.clone();
        let _supervisor = Supervisor::start(
            command,
            log.clone(),
            Box::new(move || {
                let _ = reader.ingest_connector_audit();
            }),
        )?;
        eprintln!(
            "by: the gateway serves {} at {}; its log is {} (interrupt to stop)",
            bundles(yard.root(), &config).display(),
            gw.url,
            log.display()
        );
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
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
        json!({"started": true, "pid": started.pid, "url": gw.url, "listening": listening, "log": log}),
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

fn status(yard: &Yard, dir: &Path, as_json: bool) -> Outcome {
    let (config, gw) = require(yard)?;
    let running = Background::running(dir);
    let listening = gateway::listening(&gw.url);
    let root = bundles(yard.root(), &config);
    let served = connectors::packager::discover(&root);
    let kids = KeyRing::load(&gw.key_file)
        .map(|k| k.kids())
        .unwrap_or_default();
    if as_json {
        return print(&json::text(&json!({
            "url": gw.url,
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
    let mut out = format!("gateway   {}\n", gw.url);
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
/// `anvil connect <bundles> <connector>` as the person, with a short-lived
/// token that names them and grants nothing. The gateway answers with an
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
    let (config, gw) = require(&yard)?;
    let token = gw.person_token(None, Duration::from_secs(600))?;
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
    let _ = std::fs::remove_file(&file);
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
