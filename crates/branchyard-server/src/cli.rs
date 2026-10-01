//! The `branchyard-server` command line, also run as `by serve`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use branchyard::Yard;

use clap::{ArgMatches, Args, CommandFactory, FromArgMatches, Parser, Subcommand};

use crate::config::{self, sha256_hex, Config, TlsFiles, Token, DEFAULT_TENANT, SCOPES};
use crate::serve;

const TOKEN_NEW_ABOUT: &str = "\
Generate a bearer token and the hashed credential to configure for it.

The token is printed once, in cleartext: give it to the client and discard
it; only its hash goes in the server's configuration; branchyard-server
never stores or logs the plaintext. Paste the printed `credentials` entry
into your configuration's `credentials` array (see docs/server.md).";

const AFTER_HELP: &str = "\
On start it prints 'listening on URL' to stdout. SIGINT or SIGTERM begins a
graceful shutdown; a second one stops waiting for running operations.
Harnesses run as the server's operating-system user, with no isolation
beyond it.";

/// `branchyard-server`'s command line, also `by serve` and `by worker`.
#[derive(Parser, Debug)]
#[command(
    name = "branchyard-server",
    version,
    about = "Serve Branchyard repositories over an authenticated HTTP API.",
    after_help = AFTER_HELP,
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<ServerCommand>,
    #[command(flatten)]
    flags: Flags,
}

#[derive(Subcommand, Debug)]
enum ServerCommand {
    /// Bearer tokens for the configuration's credentials
    #[command(subcommand)]
    Token(TokenCommand),
}

#[derive(Subcommand, Debug)]
enum TokenCommand {
    /// Generate a bearer token and the hashed credential to configure for it
    #[command(long_about = TOKEN_NEW_ABOUT)]
    New(TokenNew),
    /// Paired tokens and unredeemed pairing links (see docs/companion.md)
    List(TokenList),
    /// Revoke a paired token, or an unredeemed pairing link, by name
    Revoke(TokenRevoke),
}

/// Which server's companion state a token command reads: its
/// configuration, data directory or database, as `by serve` would find it.
#[derive(Args, Debug, Default)]
struct ServerPlace {
    /// The server's JSON configuration
    #[arg(short, long, value_name = "FILE", help_heading = "Server")]
    config: Option<PathBuf>,
    /// The server's data directory (default: as the configuration or by serve has it)
    #[arg(long, value_name = "DIR", help_heading = "Server")]
    data_dir: Option<PathBuf>,
    /// The server's PostgreSQL database
    #[arg(long, value_name = "URL", help_heading = "Server")]
    database: Option<String>,
}

impl ServerPlace {
    fn given(&self) -> bool {
        self.config.is_some() || self.data_dir.is_some() || self.database.is_some()
    }
}

#[derive(Args, Debug)]
struct TokenList {
    #[command(flatten)]
    place: ServerPlace,
}

#[derive(Args, Debug)]
struct TokenRevoke {
    /// The paired token's name, as `token list` shows it
    name: String,
    /// Only the one in this tenant
    #[arg(long)]
    tenant: Option<String>,
    #[command(flatten)]
    place: ServerPlace,
}

#[derive(Args, Debug)]
struct TokenNew {
    /// This credential's subject name (default: a random one)
    #[arg(long)]
    name: Option<String>,
    /// Its tenant
    #[arg(long, default_value = DEFAULT_TENANT)]
    tenant: String,
    /// Its scopes: read, run, merge, admin (default: all four)
    #[arg(long, value_name = "S,...", value_parser = names)]
    scopes: Option<Names>,
    /// Its own repository allowlist, narrower than its tenant's (default: none, meaning
    /// whatever its tenant allows)
    #[arg(long = "repo", value_name = "R,...", value_parser = names)]
    repos: Option<Names>,
    /// Print a one-time pairing link (and a QR code) for the web companion instead: opened
    /// once, it gets a token with these scopes that expires after --ttl
    #[arg(long, help_heading = "Pairing")]
    link: bool,
    /// With --link: how long the paired token lasts, such as 15m, 8h or 7d (default 24h)
    #[arg(long, value_name = "DURATION", value_parser = crate::companion::link::duration, help_heading = "Pairing")]
    ttl: Option<Duration>,
    /// With --link: how long the link may be opened, once (default 10m, at most 1h)
    #[arg(long, value_name = "DURATION", value_parser = crate::companion::link::duration, help_heading = "Pairing")]
    code_ttl: Option<Duration>,
    /// With --link: the server's URL as the phone reaches it (default: public_url, else
    /// http(s)://<listen>)
    #[arg(long, value_name = "URL", help_heading = "Pairing")]
    public_url: Option<String>,
    /// With --link: do not draw the QR code
    #[arg(long, help_heading = "Pairing")]
    no_qr: bool,
    #[command(flatten)]
    place: ServerPlace,
}

/// A comma-separated list, trimmed, without empty entries.
#[derive(Clone, Debug, Default, PartialEq)]
struct Names(Vec<String>);

fn names(text: &str) -> Result<Names, String> {
    Ok(Names(
        text.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
    ))
}

fn token_new(args: TokenNew, program: &str) -> ExitCode {
    if !args.link
        && (args.ttl.is_some()
            || args.code_ttl.is_some()
            || args.public_url.is_some()
            || args.no_qr
            || args.place.given())
    {
        eprintln!(
            "{program}: --ttl, --code-ttl, --public-url, --no-qr and the server's place go \
             with --link"
        );
        return ExitCode::from(2);
    }
    let scopes = match args.scopes {
        Some(Names(scopes)) => scopes,
        None => SCOPES.iter().map(|s| s.to_string()).collect(),
    };
    for scope in &scopes {
        if let Err(error) = config::check_scope_name(scope) {
            eprintln!("{program}: --scopes: {error}");
            return ExitCode::FAILURE;
        }
    }
    if args.link {
        let config = match place_config(&args.place) {
            Ok(config) => config,
            Err(error) => {
                eprintln!("{program}: {error}");
                return ExitCode::FAILURE;
            }
        };
        let request = crate::companion::link::LinkRequest {
            name: args.name,
            tenant: args.tenant,
            scopes,
            repos: args.repos.map(|Names(repos)| repos),
            ttl: args.ttl,
            code_ttl: args.code_ttl,
            public_url: args.public_url,
            qr: !args.no_qr,
        };
        return crate::companion::link::new_link(&config, request, program);
    }
    let name = args
        .name
        .unwrap_or_else(|| format!("token-{}", &branchyard_client::new_key()[..8]));
    let secret = new_token();
    let hash = sha256_hex(secret.as_bytes());
    eprintln!("{program}: token (printed once; give it to the client, never store it): {secret}");
    let credential = serde_json::json!({
        "token_sha256": hash,
        "tenant": args.tenant,
        "name": name,
        "scopes": scopes,
        "repos": args.repos.map(|Names(repos)| repos),
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

/// The configuration a token command's `place` names, built as `--check`
/// builds it (nothing written).
fn place_config(place: &ServerPlace) -> Result<Config, String> {
    let flags = Flags {
        config: place.config.clone(),
        data_dir: place.data_dir.clone(),
        database: place.database.clone(),
        check: true,
        ..Flags::default()
    };
    build(flags).map(|(config, _)| config)
}

/// A fresh random bearer token or secret, as `token new` and a default
/// token file get: two random keys.
pub fn new_token() -> String {
    format!(
        "{}{}",
        branchyard_client::new_key(),
        branchyard_client::new_key()
    )
}

/// The server's options. The `webhook*` and `secret` fields are as given;
/// [`parse`] turns them into `webhooks` and `secrets`.
#[derive(Args, Default, Debug, PartialEq)]
struct Flags {
    /// JSON configuration; see docs/server.md
    #[arg(short, long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Address to listen on (default: 127.0.0.1:8421)
    #[arg(long, value_name = "ADDR")]
    listen: Option<String>,
    /// Serve the repository at PATH as NAME; repeatable (default: the repository containing
    /// the current directory)
    #[arg(long = "repo", value_name = "NAME=PATH", value_parser = repo)]
    repos: Vec<(String, PathBuf)>,
    /// Operation registry and activity feeds (default: .branchyard/server in the first
    /// repository)
    #[arg(long, value_name = "DIR")]
    data_dir: Option<PathBuf>,
    /// Accept the token on the first line of FILE; repeatable (default: DATA-DIR/token,
    /// created if missing)
    #[arg(long = "token-file", value_name = "FILE")]
    token_files: Vec<PathBuf>,
    /// PEM certificate chain; serve HTTPS
    #[arg(long, value_name = "FILE", help_heading = "TLS")]
    tls_cert: Option<PathBuf>,
    /// PEM private key for --tls-cert
    #[arg(long, value_name = "FILE", help_heading = "TLS")]
    tls_key: Option<PathBuf>,
    /// Allow plain HTTP on an address other than loopback
    #[arg(long, help_heading = "TLS")]
    insecure_bind: bool,
    /// Run harness H as CMD, split on spaces; repeatable
    #[arg(
        long = "harness-command",
        value_name = "H=CMD",
        value_parser = harness_command,
        help_heading = "What requests may do"
    )]
    harness_commands: Vec<(String, Vec<String>)>,
    /// Accept a request's own command: any token holder can then choose what the server
    /// executes
    #[arg(long, help_heading = "What requests may do")]
    allow_client_commands: bool,
    /// Accept requests that name these providers besides local: microsandbox, substrate;
    /// repeatable
    #[arg(
        long = "allow-provider",
        value_name = "P,...",
        value_parser = providers,
        help_heading = "What requests may do"
    )]
    allow_provider: Vec<Names>,
    #[arg(skip)]
    allow_providers: Vec<String>,
    /// Accept delegation envelopes and spawns: harnesses get the delegation tools, with this
    /// server's by
    #[arg(long, help_heading = "What requests may do")]
    allow_delegation: bool,
    /// The by a delegating harness gets (default: this by, or by beside this executable, or
    /// on PATH)
    #[arg(long, value_name = "PATH", help_heading = "What requests may do")]
    by_path: Option<PathBuf>,
    /// Accept requests to run profiles whose tools bypass the policy
    #[arg(long, help_heading = "What requests may do")]
    allow_unapproved_tools: bool,
    /// A secret requests may name, read from this server's variable NAME or VAR, or from
    /// FILE; repeatable
    #[arg(
        long = "secret",
        value_name = "NAME[=VAR|=@FILE]",
        value_parser = secret,
        help_heading = "What requests may do"
    )]
    secret: Vec<branchyard::SecretSource>,
    #[arg(skip)]
    secrets: Vec<branchyard::SecretSource>,
    /// Keep branch state, operations and their queue in PostgreSQL (postgres://...), which
    /// several servers and workers may share; needs a build with the postgres feature
    #[arg(long, value_name = "URL", help_heading = "Operations")]
    database: Option<String>,
    /// Only run operations queued in --database, by any server on it: no listener, no
    /// webhooks (what by worker does)
    #[arg(long, help_heading = "Operations")]
    worker: bool,
    /// A label this worker carries (repeatable): it claims only operations whose
    /// require_labels are all among its labels (replaces the configuration's labels)
    #[arg(long = "label", value_name = "LABEL", help_heading = "Operations")]
    labels: Vec<String>,
    /// Say why a queued operation no live worker can claim waits, after this long
    /// (default: 60)
    #[arg(long, value_name = "SECS", value_parser = grace, help_heading = "Operations")]
    unclaimable_after: Option<Duration>,
    /// Largest artifact a publish may upload (default: 268435456)
    #[arg(long, value_name = "N", value_parser = artifact_bytes, help_heading = "Operations")]
    max_artifact_bytes: Option<u64>,
    /// Operations running at once (default: 8)
    #[arg(long, value_name = "N", value_parser = max_running, help_heading = "Operations")]
    max_running: Option<usize>,
    /// How long a claim on a queued operation lasts without renewal before another worker
    /// takes it over (default: 30)
    #[arg(long, value_name = "SECS", value_parser = lease, help_heading = "Operations")]
    operation_lease: Option<Duration>,
    /// At shutdown, wait this long for running operations (default: 60)
    #[arg(long, value_name = "SECS", value_parser = grace, help_heading = "Operations")]
    shutdown_grace: Option<Duration>,
    /// Notify URL of every served repository's activity (branch status changes, stalls,
    /// permission requests); https:// only unless loopback or --webhook-insecure
    #[arg(long, value_name = "URL", help_heading = "Webhooks")]
    webhook: Vec<String>,
    /// HMAC-SHA256 key for the most recent --webhook, signing each delivery's body
    /// (X-Branchyard-Signature)
    #[arg(long, value_name = "FILE", help_heading = "Webhooks")]
    webhook_secret: Vec<PathBuf>,
    /// Only these comma-separated kinds for the most recent --webhook: status, stall,
    /// permission_wait, merge, failure (default: every kind)
    #[arg(long, value_name = "KINDS", value_parser = webhook_events, help_heading = "Webhooks")]
    webhook_events: Vec<Names>,
    #[arg(skip)]
    webhooks: Vec<FlagWebhook>,
    /// Allow a --webhook URL that is plain http:// off loopback
    #[arg(long, help_heading = "Webhooks")]
    webhook_insecure: bool,
    /// Serve Prometheus metrics at /metrics to a principal with the admin scope or the
    /// metrics token
    #[arg(long, help_heading = "Observability")]
    metrics: bool,
    /// Also serve /metrics, and nothing else, on a plain-HTTP listener of its own (implies
    /// --metrics)
    #[arg(long, value_name = "ADDR", help_heading = "Observability")]
    metrics_addr: Option<String>,
    /// A token, on the first line of FILE, that reads /metrics and nothing else (implies
    /// --metrics)
    #[arg(long, value_name = "FILE", help_heading = "Observability")]
    metrics_token_file: Option<PathBuf>,
    /// This server's URL as webhook senders reach it, the base of each event trigger's
    /// webhook URL (default: http(s)://<listen>); see docs/triggers.md
    #[arg(long, value_name = "URL", help_heading = "Triggers")]
    public_url: Option<String>,
    /// Let every served repository's triggers run a precheck command before firing
    #[arg(long, help_heading = "Triggers")]
    allow_trigger_prechecks: bool,
    /// Serve the web companion at /app/: pairing links and Web Push (see
    /// docs/companion.md)
    #[arg(long, help_heading = "Companion")]
    app: bool,
    /// Do not log requests
    #[arg(short, long)]
    quiet: bool,
    /// Log lines on stderr as pretty text or one JSON object each (default:
    /// BRANCHYARD_LOG_FORMAT, else pretty)
    #[arg(long, value_name = "FORMAT", value_enum)]
    log_format: Option<crate::logging::LogFormat>,
    /// Load and check the configuration as serving would, print any warnings, and exit
    /// without serving or writing a file
    #[arg(long)]
    check: bool,
}

/// One `--webhook`, with the `--webhook-secret` and `--webhook-events` that
/// follow it before the next `--webhook`.
#[derive(Debug, Default, PartialEq)]
struct FlagWebhook {
    url: String,
    secret_file: Option<PathBuf>,
    events: Vec<String>,
}

fn repo(text: &str) -> Result<(String, PathBuf), String> {
    text.split_once('=')
        .filter(|(n, p)| !n.is_empty() && !p.is_empty())
        .map(|(name, path)| (name.to_owned(), path.into()))
        .ok_or_else(|| "needs NAME=PATH".into())
}

fn harness_command(text: &str) -> Result<(String, Vec<String>), String> {
    let (harness, command) = text.split_once('=').ok_or("needs HARNESS=CMD")?;
    let argv: Vec<String> = command.split_whitespace().map(str::to_owned).collect();
    if harness.is_empty() || argv.is_empty() {
        return Err("needs HARNESS=CMD".into());
    }
    Ok((harness.to_owned(), argv))
}

fn providers(text: &str) -> Result<Names, String> {
    let names = names(text)?;
    for name in &names.0 {
        config::check_provider_name(name)?;
    }
    Ok(names)
}

fn webhook_events(text: &str) -> Result<Names, String> {
    let kinds = names(text)?;
    for kind in &kinds.0 {
        config::check_webhook_event_kind(kind)?;
    }
    Ok(kinds)
}

fn secret(text: &str) -> Result<branchyard::SecretSource, String> {
    config::parse_secret(text)
}

fn artifact_bytes(text: &str) -> Result<u64, String> {
    text.parse::<u64>()
        .ok()
        .filter(|n| *n >= 1024)
        .ok_or_else(|| "needs a number of bytes, at least 1024".into())
}

fn max_running(text: &str) -> Result<usize, String> {
    text.parse::<usize>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| "needs a positive number".into())
}

fn lease(text: &str) -> Result<Duration, String> {
    text.parse::<f64>()
        .ok()
        .filter(|s| s.is_finite() && *s >= 0.1 && *s < 1e6)
        .map(Duration::from_secs_f64)
        .ok_or_else(|| "needs a number of seconds, at least 0.1".into())
}

fn grace(text: &str) -> Result<Duration, String> {
    text.parse::<f64>()
        .ok()
        .filter(|s| s.is_finite() && *s >= 0.0 && *s < 1e9)
        .map(Duration::from_secs_f64)
        .ok_or_else(|| "needs a number of seconds".into())
}

/// The command, named `program` in its usage and messages.
fn command(program: &str) -> clap::Command {
    Cli::command().bin_name(program.to_owned())
}

/// Help for `program` (`branchyard-server`, `by serve` or `by worker`).
pub fn help(program: &str) -> String {
    command(program).render_help().to_string()
}

/// Parse `args` (after the program name) as `program`'s: the server's
/// flags, or `token new`.
fn parse_cli(args: &[String], program: &str) -> Result<Cli, clap::Error> {
    let mut cmd = command(program);
    let argv = std::iter::once(program.to_owned()).chain(args.iter().cloned());
    let matches = cmd.try_get_matches_from_mut(argv)?;
    let mut cli = Cli::from_arg_matches(&matches)?;
    if cli.command.is_none() {
        let flags = &mut cli.flags;
        let fail = |message: String| {
            clap::Error::raw(clap::error::ErrorKind::ArgumentConflict, message).format(&mut cmd)
        };
        flags.webhooks = webhooks(&matches, flags).map_err(fail)?;
        flags.allow_providers = std::mem::take(&mut flags.allow_provider)
            .into_iter()
            .flat_map(|Names(names)| names)
            .collect();
        let dir = std::env::current_dir().map_err(|e| {
            clap::Error::raw(clap::error::ErrorKind::Io, e.to_string())
                .format(&mut command(program))
        })?;
        flags.secrets = std::mem::take(&mut flags.secret)
            .into_iter()
            .map(|secret| config::resolve_secret(secret, &dir))
            .collect();
    }
    Ok(cli)
}

/// `--webhook-secret` and `--webhook-events` belong to the `--webhook`
/// before them on the command line.
fn webhooks(matches: &ArgMatches, flags: &Flags) -> Result<Vec<FlagWebhook>, String> {
    let indices = |id: &str| -> Vec<usize> {
        matches
            .indices_of(id)
            .map(|i| i.collect())
            .unwrap_or_default()
    };
    let urls = indices("webhook");
    let mut webhooks: Vec<FlagWebhook> = flags
        .webhook
        .iter()
        .map(|url| FlagWebhook {
            url: url.clone(),
            ..FlagWebhook::default()
        })
        .collect();
    let owner = |at: usize, flag: &str| -> Result<usize, String> {
        urls.iter()
            .rposition(|url| *url < at)
            .ok_or_else(|| format!("{flag} needs a --webhook before it"))
    };
    for (at, file) in indices("webhook_secret")
        .into_iter()
        .zip(&flags.webhook_secret)
    {
        let webhook = &mut webhooks[owner(at, "--webhook-secret")?];
        if webhook.secret_file.is_some() {
            return Err("--webhook-secret given twice for one --webhook".into());
        }
        webhook.secret_file = Some(file.clone());
    }
    for (at, kinds) in indices("webhook_events")
        .into_iter()
        .zip(&flags.webhook_events)
    {
        let webhook = &mut webhooks[owner(at, "--webhook-events")?];
        if !webhook.events.is_empty() {
            return Err("--webhook-events given twice for one --webhook".into());
        }
        webhook.events = kinds.0.clone();
    }
    Ok(webhooks)
}

/// Whether `args` settle their own configuration file: `--config` (or
/// `-c`) given on the command line, or a command line that needs none or
/// does not parse (`--help`, `--version`, `token new`, a usage error the
/// server reports itself). `by serve` adds `branchyard.toml`'s
/// `[serve] config` only when this is false. Asks clap where `config`'s
/// value came from, so a spelling it accepts is never missed.
pub fn names_config(args: &[String]) -> bool {
    let argv = std::iter::once("branchyard-server".to_owned()).chain(args.iter().cloned());
    match Cli::command().try_get_matches_from(argv) {
        Ok(matches) => match matches.subcommand() {
            // `token list`, `token revoke` and `token new --link` read the
            // server's state: they take `[serve] config` (appended after
            // them) unless they name their own place.
            Some(("token", token)) => match token.subcommand() {
                Some(("new", new)) if !new.get_flag("link") => true,
                Some((_, sub)) => ["config", "data_dir", "database"]
                    .iter()
                    .any(|id| sub.value_source(id) == Some(clap::parser::ValueSource::CommandLine)),
                None => true,
            },
            Some(_) => true,
            None => matches.value_source("config") == Some(clap::parser::ValueSource::CommandLine),
        },
        Err(_) => true,
    }
}

/// The server's flags from `args`, as `branchyard-server` would parse them.
#[cfg(test)]
fn parse(args: &[String]) -> Result<Flags, String> {
    parse_cli(args, "branchyard-server")
        .map(|cli| cli.flags)
        .map_err(|e| e.to_string())
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
    let token = new_token();
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
    tracing::info!(
        message = %message.replace("{path}", &path.display().to_string()),
        "wrote a secret file"
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
    // A worker serves no requests, so it needs no credential: it runs each
    // operation as the principal that admitted it.
    if tokens.is_empty()
        && token_files.is_empty()
        && partial.credentials.is_empty()
        && !flags.worker
    {
        let path = data_dir.join("token");
        if flags.check {
            // A check writes nothing: note the token serving would create,
            // and check the rest as if it existed.
            if !path.exists() {
                warnings.push(format!(
                    "serving would create a token in {}",
                    path.display()
                ));
                tokens.push(Token {
                    name: "default".into(),
                    secret: new_token(),
                });
            } else {
                token_files.push(path);
            }
        } else {
            default_token(&path)?;
            token_files.push(path);
        }
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
    config.principals = partial.principals;
    config.credentials = partial.credentials;
    config.tenants = partial.tenants;
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
    if let Some(lease) = flags.operation_lease {
        config.operation_lease = lease;
    }
    config.worker_only = flags.worker;
    config.labels = match flags.labels.is_empty() {
        true => partial.labels,
        false => flags.labels,
    };
    config.labels.sort();
    config.labels.dedup();
    config.unclaimable_after = flags
        .unclaimable_after
        .or(partial.unclaimable_after)
        .unwrap_or(config.unclaimable_after);
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
    config.allow_workspace_scripts = partial.allow_workspace_scripts;
    config.secrets = partial.secrets;
    config
        .secrets
        .extend(flags.secrets.into_iter().map(|s| (s.name.clone(), s)));
    config.database = flags.database.or(partial.database);
    if let Some(aging) = partial.aging {
        config.aging = aging;
    }
    if let Some(window) = partial.fair_share_window {
        config.fair_share_window = window;
    }
    config.metrics = partial.metrics;
    if flags.metrics || flags.metrics_addr.is_some() || flags.metrics_token_file.is_some() {
        let mut metrics = config.metrics.take().unwrap_or(config::MetricsConfig {
            listen: None,
            token_sha256: None,
        });
        if let Some(addr) = &flags.metrics_addr {
            metrics.listen = Some(config::parse_listen(addr)?);
        }
        if let Some(path) = &flags.metrics_token_file {
            let token = config::read_token_file(path, &mut warnings)?;
            if token.len() < 16 {
                return Err(format!(
                    "the metrics token in {} is shorter than 16 characters",
                    path.display()
                ));
            }
            metrics.token_sha256 = Some(config::sha256_hex(token.as_bytes()));
        }
        config.metrics = Some(metrics);
    }
    config.log_requests = !flags.quiet;
    config.webhooks = partial.webhooks;
    for (i, webhook) in flags.webhooks.into_iter().enumerate() {
        let secret = match webhook.secret_file {
            Some(path) => config::read_token_file(&path, &mut warnings)?,
            None if flags.check => new_token(),
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
    config.connectors = partial.connectors;
    config.triggers.public_url = flags.public_url.or(partial.public_url);
    config.app = partial.app.unwrap_or_default();
    config.app.enabled |= flags.app;
    config.triggers.allow_prechecks = match flags.allow_trigger_prechecks {
        true => config::WorkspaceScripts::All,
        false => partial.allow_trigger_prechecks,
    };
    if flags.check {
        for (name, path) in &config.repos {
            if !path.is_dir() {
                warnings.push(format!(
                    "repository {name}: {} is not a directory",
                    path.display()
                ));
            }
        }
        if let Some(tls) = &config.tls {
            for path in [&tls.cert, &tls.key] {
                if !path.is_file() {
                    warnings.push(format!("TLS file {} does not exist yet", path.display()));
                }
            }
        }
    }
    Ok((config, warnings))
}

/// `--check`: build and validate the configuration `args` describe as
/// serving would, without writing anything. Returns its warnings, or why
/// it would not serve. `by init server` checks every configuration it
/// writes with this.
pub fn check(args: &[String]) -> Result<Vec<String>, String> {
    let mut flags = parse_cli(args, "branchyard-server")
        .map_err(|e| e.to_string().trim_end().to_owned())?
        .flags;
    flags.check = true;
    check_flags(flags)
}

/// The configuration `args` would serve, built as `--check` builds it:
/// nothing is written (no default token or secret is made). For tools
/// that open a server's stores without serving, such as `by trigger`.
pub fn resolve(args: &[String]) -> Result<Config, String> {
    let mut flags = parse_cli(args, "branchyard-server")
        .map_err(|e| e.to_string().trim_end().to_owned())?
        .flags;
    flags.check = true;
    let (config, _) = build(flags)?;
    Ok(config)
}

fn check_flags(flags: Flags) -> Result<Vec<String>, String> {
    let (config, mut warnings) = build(flags)?;
    if let Some(warning) = config.validate()? {
        warnings.push(warning);
    }
    Ok(warnings)
}

/// SIGINT and SIGTERM, which both shut the server down the same way.
/// Registered before the server starts, so one that arrives during startup
/// is kept rather than lost (a container's PID 1 ignores a signal it has no
/// handler for, and `docker stop` then waits for its timeout) and stops the
/// server as soon as it is serving.
struct Signals {
    #[cfg(unix)]
    streams: Option<(tokio::signal::unix::Signal, tokio::signal::unix::Signal)>,
}

impl Signals {
    /// Needs a Tokio runtime.
    fn new() -> Signals {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let streams = signal(SignalKind::interrupt())
                .and_then(|interrupt| Ok((interrupt, signal(SignalKind::terminate())?)))
                .ok();
            Signals { streams }
        }
        #[cfg(not(unix))]
        Signals {}
    }

    /// The next SIGINT or SIGTERM (Ctrl-C where there are no signals).
    async fn recv(&mut self) {
        #[cfg(unix)]
        if let Some((interrupt, terminate)) = &mut self.streams {
            tokio::select! {
                _ = interrupt.recv() => {}
                _ = terminate.recv() => {}
            }
            return;
        }
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Run the server with `args` (after the program name); `program` names it
/// in messages.
pub fn main(args: &[String], program: &str) -> ExitCode {
    let flags = match parse_cli(args, program) {
        Ok(Cli {
            command: Some(ServerCommand::Token(TokenCommand::New(args))),
            ..
        }) => return token_new(args, program),
        Ok(Cli {
            command: Some(ServerCommand::Token(TokenCommand::List(args))),
            ..
        }) => {
            return match place_config(&args.place) {
                Ok(config) => crate::companion::link::list(&config, program),
                Err(error) => {
                    eprintln!("{program}: {error}");
                    ExitCode::FAILURE
                }
            }
        }
        Ok(Cli {
            command: Some(ServerCommand::Token(TokenCommand::Revoke(args))),
            ..
        }) => {
            return match place_config(&args.place) {
                Ok(config) => crate::companion::link::revoke(
                    &config,
                    &args.name,
                    args.tenant.as_deref(),
                    program,
                ),
                Err(error) => {
                    eprintln!("{program}: {error}");
                    ExitCode::FAILURE
                }
            }
        }
        Ok(cli) => cli.flags,
        // Help and --version come this way too, to stdout with exit 0;
        // usage errors go to stderr with exit 2.
        Err(error) => {
            let _ = error.print();
            return ExitCode::from(error.exit_code() as u8);
        }
    };
    if flags.check {
        return match check_flags(flags) {
            Ok(warnings) => {
                for warning in &warnings {
                    eprintln!("{program}: warning: {warning}");
                }
                println!("configuration ok");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("{program}: {error}");
                ExitCode::FAILURE
            }
        };
    }
    // `--quiet` keeps meaning "warn and above" for anyone who does not set
    // `BRANCHYARD_LOG`/`RUST_LOG` themselves; see `logging::init`.
    crate::logging::init(flags.quiet, flags.log_format);
    let (config, warnings) = match build(flags) {
        Ok(built) => built,
        Err(error) => {
            tracing::error!(%error, "cannot build the configuration");
            return ExitCode::FAILURE;
        }
    };
    for warning in warnings {
        tracing::warn!(%warning, "configuration warning");
    }
    match config.validate() {
        Ok(None) => {}
        Ok(Some(warning)) => {
            tracing::warn!(%warning, "configuration warning");
        }
        Err(error) => {
            tracing::error!(%error, "invalid configuration");
            return ExitCode::FAILURE;
        }
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(%error, "cannot start the async runtime");
            return ExitCode::FAILURE;
        }
    };
    let repos: Vec<String> = config.repos.iter().map(|(n, _)| n.clone()).collect();
    let code = runtime.block_on(async move {
        let mut signals = Signals::new();
        let running = match serve::start(config).await {
            Ok(running) => running,
            Err(error) => {
                tracing::error!(%error, "cannot start serving");
                return ExitCode::FAILURE;
            }
        };
        if running.is_worker() {
            println!("working on {}", repos.join(", "));
            let _ = std::io::stdout().flush();
            tracing::info!(
                repos = %repos.join(", "),
                "running operations queued in the database; harnesses run as this user, \
                 with no isolation beyond it"
            );
        } else {
            println!("listening on {}", running.url());
            let _ = std::io::stdout().flush();
            tracing::info!(
                repos = %repos.join(", "),
                url = %running.url(),
                "serving; harnesses run as this user, with no isolation beyond it"
            );
        }
        let handle = running.handle();
        tokio::spawn(async move {
            signals.recv().await;
            tracing::info!(
                "shutting down; running operations may finish (signal again to stop waiting)"
            );
            handle.shutdown();
            signals.recv().await;
            handle.force();
        });
        let stopped = running.wait().await;
        if stopped.interrupted > 0 {
            tracing::warn!(
                interrupted = stopped.interrupted,
                "recorded unfinished operation(s) as interrupted"
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
             --by-path /opt/by --allow-unapproved-tools --database postgres://u@h/d \
             --worker --operation-lease 2.5 --label gpu --label=linux --unclaimable-after 5",
        ))
        .unwrap();
        assert!(flags.worker);
        assert_eq!(flags.labels, ["gpu", "linux"]);
        assert_eq!(flags.unclaimable_after, Some(Duration::from_secs(5)));
        assert_eq!(flags.operation_lease, Some(Duration::from_millis(2500)));
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
            ("--bogus", "unexpected argument '--bogus'"),
            ("--listen", "a value is required for '--listen <ADDR>'"),
            ("--repo x", "needs NAME=PATH"),
            ("--config a --config b", "cannot be used multiple times"),
            ("--max-running 0", "positive number"),
            (
                "--insecure-bind=1",
                "unexpected value '1' for '--insecure-bind'",
            ),
            ("--allow-provider docker", "\"docker\" is not a provider"),
            (
                "--allow-delegation=yes",
                "unexpected value 'yes' for '--allow-delegation'",
            ),
            ("extra", "unrecognized subcommand 'extra'"),
            ("--operation-lease 0", "at least 0.1"),
            ("--worker=1", "unexpected value '1' for '--worker'"),
            ("token new --listen x", "unexpected argument '--listen'"),
            ("--listen x token new", "cannot be used with"),
        ] {
            let got = parse(&args(line)).unwrap_err();
            assert!(got.contains(error), "{line}: {got}");
        }
        // Short forms, and the server's help under each of its names.
        let flags = parse(&args("-c conf.json -q")).unwrap();
        assert_eq!(flags.config, Some(PathBuf::from("conf.json")));
        assert!(flags.quiet);
        assert_eq!(flags.log_format, None);
        let json = parse(&args("--log-format json")).unwrap();
        assert_eq!(json.log_format, Some(crate::logging::LogFormat::Json));
        assert!(parse(&args("--log-format yaml"))
            .unwrap_err()
            .contains("invalid value 'yaml' for '--log-format <FORMAT>'"));
        assert!(help("by serve").contains("--log-format <FORMAT>"));
        assert!(help("by worker").contains("Usage: by worker [OPTIONS]"));
        assert!(help("branchyard-server").contains("--webhook-events <KINDS>"));
        command("branchyard-server").debug_assert();
    }

    #[test]
    fn token_new_takes_its_own_flags() {
        let cli = parse_cli(
            &args("token new --name ci --tenant acme --scopes read,run --repo app,docs"),
            "branchyard-server",
        )
        .unwrap();
        let Some(ServerCommand::Token(TokenCommand::New(new))) = cli.command else {
            panic!("not token new")
        };
        assert_eq!(new.name.as_deref(), Some("ci"));
        assert_eq!(new.tenant, "acme");
        assert_eq!(new.scopes, Some(Names(vec!["read".into(), "run".into()])));
        assert_eq!(new.repos, Some(Names(vec!["app".into(), "docs".into()])));
        let cli = parse_cli(&args("token new"), "by serve").unwrap();
        let Some(ServerCommand::Token(TokenCommand::New(new))) = cli.command else {
            panic!("not token new")
        };
        assert_eq!((new.tenant.as_str(), new.scopes), (DEFAULT_TENANT, None));
        let help = parse_cli(&args("token new --help"), "by serve")
            .unwrap_err()
            .to_string();
        assert!(help.contains("printed once"), "{help}");
        assert!(
            help.contains("Usage: by serve token new [OPTIONS]"),
            "{help}"
        );
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
                "--webhook https://a.example --webhook-events stall --webhook-events merge",
                "given twice",
            ),
            (
                "--webhook-secret f --webhook https://a.example",
                "needs a --webhook before it",
            ),
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

    #[test]
    fn a_configuration_files_tenants_and_credentials_reach_the_server() {
        let dir =
            std::env::temp_dir().join(format!("branchyard-cli-tenants-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let hash = crate::config::sha256_hex(b"acme-token-0123456789");
        let file = dir.join("config.json");
        std::fs::write(
            &file,
            serde_json::json!({
                "data_dir": "data",
                "repos": { "app": "app" },
                "tokens": [{ "name": "ops", "token": "ops-token-0123456789", "tenant": "ops" }],
                "credentials": [{ "token_sha256": hash, "tenant": "acme", "name": "ci" }],
                "tenants": { "acme": { "repos": ["app"], "max_running": 2, "max_branches": 5 } }
            })
            .to_string(),
        )
        .unwrap();
        let config_arg = format!("--config {}", file.display());
        let (config, _) = build(parse(&args(&config_arg)).unwrap()).unwrap();
        assert_eq!(config.credentials.len(), 1);
        assert_eq!(config.credentials[0].principal.tenant, "acme");
        assert_eq!(config.principals["ops"].tenant, "ops");
        assert_eq!(config.tenant_policy("acme").max_running, Some(2));
        assert_eq!(config.tenant_policy("acme").max_branches, Some(5));
        assert!(
            !dir.join("data/token").exists(),
            "no default token beside credentials"
        );

        // A worker needs no credential at all.
        std::fs::write(
            &file,
            serde_json::json!({ "data_dir": "data", "repos": { "app": "app" } }).to_string(),
        )
        .unwrap();
        let line = format!("{config_arg} --worker --database postgres://db/branchyard");
        let (config, _) = build(parse(&args(&line)).unwrap()).unwrap();
        assert!(config.tokens.is_empty() && config.credentials.is_empty());
        assert!(!dir.join("data/token").exists());
        assert!(config.validate().is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_validates_without_writing_anything() {
        let dir = std::env::temp_dir().join(format!("branchyard-cli-check-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("app")).unwrap();
        let file = dir.join("config.json");
        std::fs::write(
            &file,
            serde_json::json!({ "data_dir": "data", "repos": { "app": "app" } }).to_string(),
        )
        .unwrap();
        let line = format!("--config {}", file.display());
        let warnings = check(&args(&line)).unwrap();
        assert!(
            warnings.iter().any(|w| w.contains("would create a token")),
            "{warnings:?}"
        );
        assert!(!dir.join("data").exists(), "a check writes nothing");
        std::fs::write(
            &file,
            serde_json::json!({ "listen": "0.0.0.0:8421", "data_dir": "data", "repos": { "app": "app" } })
                .to_string(),
        )
        .unwrap();
        assert!(check(&args(&line)).unwrap_err().contains("--insecure-bind"));
        let insecure = format!("{line} --insecure-bind");
        assert!(check(&args(&insecure))
            .unwrap()
            .iter()
            .any(|w| w.contains("WARNING")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn names_config_follows_clap() {
        for (line, names) in [
            ("", false),
            ("--worker --database postgres://h/d", false),
            ("--check", false),
            ("--config c.json", true),
            ("--config=c.json --check", true),
            ("-c c.json", true),
            ("-cc.json", true),
            ("--help", true),
            ("-V", true),
            ("token new --tenant a", true),
            ("--bogus", true),
        ] {
            assert_eq!(names_config(&args(line)), names, "{line:?}");
        }
    }

    #[test]
    fn check_is_a_flag() {
        assert!(parse(&args("--check --config c.json")).unwrap().check);
        assert!(parse(&args("--check=yes")).is_err());
    }
}
