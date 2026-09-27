//! The `branchyard-server` command line, also run as `by serve`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use branchyard::Yard;

use crate::config::{self, Config, TlsFiles, Token};
use crate::serve;

pub const USAGE: &str = "\
Serve Branchyard repositories over an authenticated HTTP API.

Usage: branchyard-server [options]
       by serve [options]

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
    max_running: Option<usize>,
    shutdown_grace: Option<Duration>,
    quiet: bool,
    help: bool,
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
        "branchyard-server: created a token in {}; clients pass --token-file with it",
        path.display()
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
    config.log_requests = !flags.quiet;
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
             --harness-command gemini-cli=/bin/agent --max-running 2 --shutdown-grace 1.5",
        ))
        .unwrap();
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
            ("extra", "unexpected argument"),
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
