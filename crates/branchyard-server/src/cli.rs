//! The `branchyard-server` command line, also run as `by serve`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use branchyard::Yard;

use crate::config::{self, sha256_hex, Config, TlsFiles, Token, DEFAULT_TENANT, SCOPES};
use crate::serve;

pub const TOKEN_USAGE: &str = "\
Generate a bearer token and the hashed credential to configure for it.

Usage: branchyard-server token new [options]

The token is printed once, in cleartext: give it to the client and discard
it; only its hash goes in the server's configuration; branchyard-server
never stores or logs the plaintext. Paste the printed `credentials` entry
into your configuration's `credentials` array (see `docs/server.md`).

Options:
  --name NAME       This credential's subject name (default: a random one)
  --tenant TENANT   Its tenant (default: 'default')
  --scopes S,...    Its scopes: read, run, merge, admin (default: all four)
  --repo R,...      Its own repository allowlist, narrower than its
                     tenant's (default: none, meaning whatever its tenant
                     allows)
  -h, --help        Show this help
";

fn token_new(args: &[String], program: &str) -> ExitCode {
    let mut name = None;
    let mut tenant = DEFAULT_TENANT.to_owned();
    let mut scopes: Vec<String> = SCOPES.iter().map(|s| s.to_string()).collect();
    let mut repos: Option<Vec<String>> = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n, Some(v.to_owned())),
            _ => (arg.as_str(), None),
        };
        let mut value = |what: &str| -> Result<String, String> {
            match &inline {
                Some(v) => Ok(v.clone()),
                None => args
                    .next()
                    .cloned()
                    .ok_or_else(|| format!("{flag} needs a value {what}")),
            }
        };
        let result = match flag {
            "-h" | "--help" => {
                print!("{TOKEN_USAGE}");
                return ExitCode::SUCCESS;
            }
            "--name" => value("NAME").map(|v| name = Some(v)),
            "--tenant" => value("TENANT").map(|v| tenant = v),
            "--scopes" => value("S,...").map(|v| {
                scopes = v
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect();
            }),
            "--repo" => value("R,...").map(|v| {
                repos = Some(
                    v.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect(),
                );
            }),
            other => Err(format!("unknown option {other}")),
        };
        if let Err(error) = result {
            eprintln!("{program}: {error}\nTry '{program} token new --help'.");
            return ExitCode::from(2);
        }
    }
    for scope in &scopes {
        if let Err(error) = config::check_scope_name(scope) {
            eprintln!("{program}: --scopes: {error}");
            return ExitCode::FAILURE;
        }
    }
    let name = name.unwrap_or_else(|| format!("token-{}", &branchyard_client::new_key()[..8]));
    let secret = format!(
        "{}{}",
        branchyard_client::new_key(),
        branchyard_client::new_key()
    );
    let hash = sha256_hex(secret.as_bytes());
    eprintln!("{program}: token (printed once; give it to the client, never store it): {secret}");
    let credential = serde_json::json!({
        "token_sha256": hash,
        "tenant": tenant,
        "name": name,
        "scopes": scopes,
        "repos": repos,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&credential).unwrap_or_default()
    );
    eprintln!(
        "{program}: add the object above to your configuration's top-level 'credentials' array"
    );
    ExitCode::SUCCESS
}

pub const USAGE: &str = "\
Serve Branchyard repositories over an authenticated HTTP API.

Usage: branchyard-server [options]
       by serve [options]
       branchyard-server token new [options]   (see 'branchyard-server token new --help')

Options:
  --config FILE             JSON configuration; see docs/server.md
  --listen ADDR             Address to listen on (default: 127.0.0.1:8421)
  --repo NAME=PATH          Serve the repository at PATH as NAME; repeatable
                            (default: the repository containing the current directory)
  --data-dir DIR            Operation registry and activity feeds
                            (default: .branchyard/server in the first repository)
  --token-file FILE         Accept the token on the first line of FILE; repeatable
                            (default: DATA-DIR/token, created if missing)
  --tls-cert FILE           PEM certificate chain; serve HTTPS
  --tls-key FILE            PEM private key for --tls-cert
  --insecure-bind           Allow plain HTTP on an address other than loopback
  --harness-command H=CMD   Run harness H as CMD, split on spaces; repeatable
  --allow-client-commands   Accept a request's own command: any token holder can then
                            choose what the server executes
  --allow-provider P,...    Accept requests that name these providers besides local:
                            microsandbox, substrate; repeatable
  --allow-delegation        Accept delegation envelopes and spawns: harnesses get the
                            delegation tools, with this server's by
  --by-path PATH            The by a delegating harness gets (default: this by, or by
                            beside this executable, or on PATH)
  --allow-unapproved-tools  Accept requests to run profiles whose tools bypass the policy
  --secret NAME[=VAR|=@FILE]
                            A secret requests may name, read from this server's
                            variable NAME or VAR, or from FILE; repeatable
  --database URL            Keep branch state and operations in PostgreSQL
                            (postgres://...); needs a build with the postgres feature
  --max-artifact-bytes N    Largest artifact a publish may upload (default: 268435456)
  --webhook URL             Notify URL of every served repository's activity
                            (branch status changes, stalls, permission requests);
                            https:// only unless loopback or --webhook-insecure
  --webhook-secret FILE     HMAC-SHA256 key for the most recent --webhook, signing
                            each delivery's body (X-Branchyard-Signature)
  --webhook-events KINDS    Only these comma-separated kinds for the most recent
                            --webhook: status, stall, permission_wait, merge,
                            failure (default: every kind)
  --webhook-insecure        Allow a --webhook URL that is plain http:// off loopback
  --max-running N           Operations running at once (default: 8)
  --shutdown-grace SECS     At shutdown, wait this long for running operations (default: 60)
  --quiet                   Do not log requests
  -h, --help                Show this help

On start it prints 'listening on URL' to stdout. SIGINT or SIGTERM begins a
graceful shutdown; a second one stops waiting for running operations.
Harnesses run as the server's operating-system user, with no isolation
beyond it.
";

#[derive(Default, Debug, PartialEq)]
struct Flags {
    config: Option<PathBuf>,
    listen: Option<String>,
    repos: Vec<(String, PathBuf)>,
    data_dir: Option<PathBuf>,
    token_files: Vec<PathBuf>,
    tls_cert: Option<PathBuf>,
    tls_key: Option<PathBuf>,
    insecure_bind: bool,
    harness_commands: Vec<(String, Vec<String>)>,
    allow_client_commands: bool,
    allow_providers: Vec<String>,
    allow_delegation: bool,
    by_path: Option<PathBuf>,
    allow_unapproved_tools: bool,
    secrets: Vec<branchyard::SecretSource>,
    database: Option<String>,
    max_artifact_bytes: Option<u64>,
    max_running: Option<usize>,
    shutdown_grace: Option<Duration>,
    webhooks: Vec<FlagWebhook>,
    webhook_insecure: bool,
    quiet: bool,
    help: bool,
}

/// One `--webhook`, with the `--webhook-secret` and `--webhook-events` that
/// follow it before the next `--webhook`.
#[derive(Debug, Default, PartialEq)]
struct FlagWebhook {
    url: String,
    secret_file: Option<PathBuf>,
    events: Vec<String>,
}

fn parse(args: &[String]) -> Result<Flags, String> {
    let mut flags = Flags::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value.to_owned())),
            _ => (arg.as_str(), None),
        };
        let mut value = |what: &str| -> Result<String, String> {
            match &inline {
                Some(value) => Ok(value.clone()),
                None => args
                    .next()
                    .cloned()
                    .ok_or_else(|| format!("{name} needs a value {what}")),
            }
        };
        let once = |seen: bool| match seen {
            true => Err(format!("{name} given twice")),
            false => Ok(()),
        };
        match name {
            "-h" | "--help" => flags.help = true,
            "--config" => {
                once(flags.config.is_some())?;
                flags.config = Some(value("FILE")?.into());
            }
            "--listen" => {
                once(flags.listen.is_some())?;
                flags.listen = Some(value("ADDR")?);
            }
            "--repo" => {
                let text = value("NAME=PATH")?;
                let (name, path) = text
                    .split_once('=')
                    .filter(|(n, p)| !n.is_empty() && !p.is_empty())
                    .ok_or_else(|| format!("--repo needs NAME=PATH, not {text:?}"))?;
                flags.repos.push((name.to_owned(), path.into()));
            }
            "--data-dir" => {
                once(flags.data_dir.is_some())?;
                flags.data_dir = Some(value("DIR")?.into());
            }
            "--token-file" => flags.token_files.push(value("FILE")?.into()),
            "--tls-cert" => {
                once(flags.tls_cert.is_some())?;
                flags.tls_cert = Some(value("FILE")?.into());
            }
            "--tls-key" => {
                once(flags.tls_key.is_some())?;
                flags.tls_key = Some(value("FILE")?.into());
            }
            "--insecure-bind" if inline.is_none() => flags.insecure_bind = true,
            "--allow-client-commands" if inline.is_none() => flags.allow_client_commands = true,
            "--quiet" if inline.is_none() => flags.quiet = true,
            "--allow-delegation" if inline.is_none() => flags.allow_delegation = true,
            "--allow-unapproved-tools" if inline.is_none() => flags.allow_unapproved_tools = true,
            "--allow-provider" => {
                let text = value("PROVIDER,...")?;
                for name in text.split(',').map(str::trim).filter(|n| !n.is_empty()) {
                    config::check_provider_name(name)?;
                    flags.allow_providers.push(name.to_owned());
                }
            }
            "--secret" => {
                let secret = config::parse_secret(&value("NAME[=VAR|=@FILE]")?)
                    .map_err(|e| format!("--secret: {e}"))?;
                let dir = std::env::current_dir().map_err(|e| e.to_string())?;
                flags.secrets.push(config::resolve_secret(secret, &dir));
            }
            "--by-path" => {
                once(flags.by_path.is_some())?;
                flags.by_path = Some(value("PATH")?.into());
            }
            "--database" => {
                once(flags.database.is_some())?;
                flags.database = Some(value("URL")?);
            }
            "--harness-command" => {
                let text = value("HARNESS=CMD")?;
                let (harness, command) = text
                    .split_once('=')
                    .ok_or_else(|| format!("--harness-command needs HARNESS=CMD, not {text:?}"))?;
                let argv: Vec<String> = command.split_whitespace().map(str::to_owned).collect();
                if harness.is_empty() || argv.is_empty() {
                    return Err(format!("--harness-command needs HARNESS=CMD, not {text:?}"));
                }
                flags.harness_commands.push((harness.to_owned(), argv));
            }
            "--max-artifact-bytes" => {
                once(flags.max_artifact_bytes.is_some())?;
                let text = value("N")?;
                let n = text.parse::<u64>().ok().filter(|n| *n >= 1024).ok_or_else(|| {
                    format!("--max-artifact-bytes needs a number of bytes, at least 1024, not {text:?}")
                })?;
                flags.max_artifact_bytes = Some(n);
            }
            "--max-running" => {
                once(flags.max_running.is_some())?;
                let text = value("N")?;
                let n = text
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| {
                        format!("--max-running needs a positive number, not {text:?}")
                    })?;
                flags.max_running = Some(n);
            }
            "--webhook" => {
                flags.webhooks.push(FlagWebhook {
                    url: value("URL")?,
                    secret_file: None,
                    events: Vec::new(),
                });
            }
            "--webhook-secret" => {
                let path: PathBuf = value("FILE")?.into();
                let webhook = flags
                    .webhooks
                    .last_mut()
                    .ok_or("--webhook-secret needs a --webhook before it")?;
                once(webhook.secret_file.is_some())?;
                webhook.secret_file = Some(path);
            }
            "--webhook-events" => {
                let text = value("KINDS")?;
                let webhook = flags
                    .webhooks
                    .last_mut()
                    .ok_or("--webhook-events needs a --webhook before it")?;
                once(!webhook.events.is_empty())?;
                for kind in text.split(',').map(str::trim).filter(|k| !k.is_empty()) {
                    config::check_webhook_event_kind(kind)?;
                    webhook.events.push(kind.to_owned());
                }
            }
            "--webhook-insecure" if inline.is_none() => flags.webhook_insecure = true,
            "--shutdown-grace" => {
                once(flags.shutdown_grace.is_some())?;
                let text = value("SECS")?;
                let secs = text
                    .parse::<f64>()
                    .ok()
                    .filter(|s| s.is_finite() && *s >= 0.0 && *s < 1e9)
                    .ok_or_else(|| {
                        format!("--shutdown-grace needs a number of seconds, not {text:?}")
                    })?;
                flags.shutdown_grace = Some(Duration::from_secs_f64(secs));
            }
            other if other.starts_with('-') => return Err(format!("unknown option {other}")),
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    Ok(flags)
}

/// A token file's contents, created with a fresh random token and mode 600
/// when missing.
fn default_token(path: &Path) -> Result<(), String> {
    default_secret(
        path,
        "created a token in {path}; clients pass --token-file with it",
    )
}

/// A file's contents, created with a fresh random secret and mode 600 when
/// missing. `message` names the file with `{path}`.
fn default_secret(path: &Path, message: &str) -> Result<(), String> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let token = format!(
        "{}{}",
        branchyard_client::new_key(),
        branchyard_client::new_key()
    );
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|e| format!("token file {}: {e}", path.display()))?;
    writeln!(file, "{token}").map_err(|e| format!("token file {}: {e}", path.display()))?;
    eprintln!(
        "branchyard-server: {}",
        message.replace("{path}", &path.display().to_string())
    );
    Ok(())
}

fn build(flags: Flags) -> Result<(Config, Vec<String>), String> {
    let mut partial = match &flags.config {
        Some(path) => config::load_file(path)?,
        None => config::Partial::default(),
    };
    let mut warnings = std::mem::take(&mut partial.warnings);
    let mut repos = partial.repos;
    repos.extend(flags.repos);
    if repos.is_empty() {
        if flags.config.is_some() {
            return Err("the configuration serves no repositories".into());
        }
        let yard = Yard::open(".").map_err(|e| e.to_string())?;
        let root = yard.root().to_path_buf();
        repos.push((config::repo_name_for(&root), root));
    }
    let data_dir = match flags.data_dir.or(partial.data_dir) {
        Some(dir) => dir,
        None if flags.config.is_some() => {
            return Err("the configuration needs data_dir".into());
        }
        None => repos[0].1.join(".branchyard").join("server"),
    };
    let mut tokens = partial.tokens;
    let mut token_files = flags.token_files;
    if tokens.is_empty() && token_files.is_empty() {
        let path = data_dir.join("token");
        default_token(&path)?;
        token_files.push(path);
    }
    for (i, path) in token_files.iter().enumerate() {
        let secret = config::read_token_file(path, &mut warnings)?;
        let name = match i {
            0 => "default".to_owned(),
            n => format!("token-{n}"),
        };
        tokens.push(Token { name, secret });
    }
    let mut config = Config::new(data_dir);
    if let Some(listen) = partial.listen {
        config.listen = listen;
    }
    if let Some(listen) = &flags.listen {
        config.listen = config::parse_listen(listen)?;
    }
    config.repos = repos;
    config.tokens = tokens;
    config.tls = match (flags.tls_cert, flags.tls_key) {
        (Some(cert), Some(key)) => Some(TlsFiles { cert, key }),
        (None, None) => partial.tls,
        _ => return Err("--tls-cert and --tls-key go together".into()),
    };
    config.insecure_bind = flags.insecure_bind;
    if let Some(bytes) = partial.max_body_bytes {
        config.max_body_bytes = bytes;
    }
    config.max_artifact_bytes = flags
        .max_artifact_bytes
        .or(partial.max_artifact_bytes)
        .unwrap_or(config.max_artifact_bytes);
    config.max_running = flags
        .max_running
        .or(partial.max_running)
        .unwrap_or(config.max_running);
    config.shutdown_grace = flags
        .shutdown_grace
        .or(partial.shutdown_grace)
        .unwrap_or(config.shutdown_grace);
    config.harness_commands = partial.harness_commands;
    config.harness_commands.extend(flags.harness_commands);
    config.allow_client_commands = partial.allow_client_commands || flags.allow_client_commands;
    config.allow_providers = partial
        .allow_providers
        .into_iter()
        .chain(flags.allow_providers)
        .collect();
    config.allow_delegation = partial.allow_delegation || flags.allow_delegation;
    config.by_path = flags.by_path.or(partial.by_path);
    config.allow_unapproved_tools = partial.allow_unapproved_tools || flags.allow_unapproved_tools;
    config.secrets = partial.secrets;
    config
        .secrets
        .extend(flags.secrets.into_iter().map(|s| (s.name.clone(), s)));
    config.database = flags.database.or(partial.database);
    config.log_requests = !flags.quiet;
    config.webhooks = partial.webhooks;
    for (i, webhook) in flags.webhooks.into_iter().enumerate() {
        let secret = match webhook.secret_file {
            Some(path) => config::read_token_file(&path, &mut warnings)?,
            None => {
                let path = config.data_dir.join(format!("webhook-{i}.secret"));
                default_secret(
                    &path,
                    "created a webhook secret in {path}; give it to the receiver to verify \
                     X-Branchyard-Signature",
                )?;
                config::read_token_file(&path, &mut warnings)?
            }
        };
        for kind in &webhook.events {
            config::check_webhook_event_kind(kind)?;
        }
        config.webhooks.push(config::WebhookConfig {
            id: webhook.url.clone(),
            url: webhook.url,
            secret,
            events: webhook.events.into_iter().collect(),
        });
    }
    config.webhook_insecure = partial.webhook_insecure || flags.webhook_insecure;
    Ok((config, warnings))
}

/// Wait for SIGINT or SIGTERM.
async fn signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Run the server with `args` (after the program name); `program` names it
/// in messages.
pub fn main(args: &[String], program: &str) -> ExitCode {
    if args.first().map(String::as_str) == Some("token") {
        return match args.get(1).map(String::as_str) {
            Some("new") => token_new(&args[2..], program),
            _ => {
                eprintln!("{program}: usage: {program} token new [options]");
                ExitCode::from(2)
            }
        };
    }
    let flags = match parse(args) {
        Ok(flags) => flags,
        Err(error) => {
            eprintln!("{program}: {error}\nTry '{program} --help'.");
            return ExitCode::from(2);
        }
    };
    if flags.help {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let (config, warnings) = match build(flags) {
        Ok(built) => built,
        Err(error) => {
            eprintln!("{program}: {error}");
            return ExitCode::FAILURE;
        }
    };
    for warning in warnings {
        eprintln!("{program}: warning: {warning}");
    }
    match config.validate() {
        Ok(None) => {}
        Ok(Some(warning)) => {
            let rule = "!".repeat(72);
            eprintln!("{rule}\n{program}: {warning}\n{rule}");
        }
        Err(error) => {
            eprintln!("{program}: {error}");
            return ExitCode::FAILURE;
        }
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("{program}: cannot start the runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    let repos: Vec<String> = config.repos.iter().map(|(n, _)| n.clone()).collect();
    let code = runtime.block_on(async move {
        let running = match serve::start(config).await {
            Ok(running) => running,
            Err(error) => {
                eprintln!("{program}: {error}");
                return ExitCode::FAILURE;
            }
        };
        println!("listening on {}", running.url());
        let _ = std::io::stdout().flush();
        eprintln!(
            "{program}: serving {} on {}; harnesses run as this user, with no isolation beyond it",
            repos.join(", "),
            running.url()
        );
        let handle = running.handle();
        let name = program.to_owned();
        tokio::spawn(async move {
            signal().await;
            eprintln!("{name}: shutting down; running operations may finish (signal again to stop waiting)");
            handle.shutdown();
            signal().await;
            handle.force();
        });
        let stopped = running.wait().await;
        if stopped.interrupted > 0 {
            eprintln!(
                "{program}: recorded {} unfinished operation(s) as interrupted",
                stopped.interrupted
            );
        }
        ExitCode::SUCCESS
    });
    // Worker threads still running an interrupted operation end with the
    // process; do not wait for them.
    runtime.shutdown_timeout(Duration::from_millis(100));
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn flags_parse() {
        let flags = parse(&args(
            "--listen 0.0.0.0:1 --repo a=/x --repo=b=/y --insecure-bind --token-file t \
             --harness-command gemini-cli=/bin/agent --max-running 2 --shutdown-grace 1.5 \
             --allow-provider substrate --allow-provider=microsandbox,local --allow-delegation \
             --by-path /opt/by --allow-unapproved-tools --database postgres://u@h/d",
        ))
        .unwrap();
        assert_eq!(
            flags.allow_providers,
            ["substrate", "microsandbox", "local"]
        );
        assert!(flags.allow_delegation && flags.allow_unapproved_tools);
        assert_eq!(flags.by_path, Some(PathBuf::from("/opt/by")));
        assert_eq!(flags.database.as_deref(), Some("postgres://u@h/d"));
        assert_eq!(flags.listen.as_deref(), Some("0.0.0.0:1"));
        assert_eq!(
            flags.repos,
            [("a".into(), "/x".into()), ("b".into(), "/y".into())]
        );
        assert!(flags.insecure_bind);
        assert_eq!(
            flags.harness_commands,
            [("gemini-cli".into(), vec!["/bin/agent".to_owned()])]
        );
        assert_eq!(flags.shutdown_grace, Some(Duration::from_millis(1500)));
        for (line, error) in [
            ("--bogus", "unknown option --bogus"),
            ("--listen", "--listen needs a value ADDR"),
            ("--repo x", "--repo needs NAME=PATH"),
            ("--config a --config b", "--config given twice"),
            ("--max-running 0", "positive number"),
            ("--insecure-bind=1", "unknown option --insecure-bind"),
            ("--allow-provider docker", "\"docker\" is not a provider"),
            (
                "--allow-delegation=yes",
                "unknown option --allow-delegation",
            ),
            ("extra", "unexpected argument"),
        ] {
            let got = parse(&args(line)).unwrap_err();
            assert!(got.contains(error), "{line}: {got}");
        }
    }

    #[test]
    fn webhook_flags_group_by_the_preceding_webhook() {
        let flags = parse(&args(
            "--webhook https://a.example/hook --webhook-secret sfile \
             --webhook-events stall,merge --webhook https://b.example/hook \
             --webhook-insecure",
        ))
        .unwrap();
        assert_eq!(
            flags.webhooks,
            [
                FlagWebhook {
                    url: "https://a.example/hook".into(),
                    secret_file: Some("sfile".into()),
                    events: vec!["stall".into(), "merge".into()],
                },
                FlagWebhook {
                    url: "https://b.example/hook".into(),
                    secret_file: None,
                    events: Vec::new(),
                },
            ]
        );
        assert!(flags.webhook_insecure);
        for (line, error) in [
            ("--webhook-secret f", "needs a --webhook before it"),
            ("--webhook-events stall", "needs a --webhook before it"),
            (
                "--webhook https://a.example --webhook-events bogus",
                "not a webhook event kind",
            ),
        ] {
            let got = parse(&args(line)).unwrap_err();
            assert!(got.contains(error), "{line}: {got}");
        }
    }

    #[test]
    fn tls_flags_go_together() {
        let flags = parse(&args("--repo a=/x --token-file /nonexistent --tls-cert c")).unwrap();
        let error = build(flags).unwrap_err();
        assert!(
            error.contains("token file") || error.contains("--tls-key"),
            "{error}"
        );
    }
}
