//! `branchyard-herdr`: a Herdr plugin that presents a Branchyard server's
//! branches in Herdr. See `plugins/herdr/README.md`.
//!
//! Herdr runs each of these from the plugin's `herdr-plugin.toml`:
//!
//! - `bridge`: follow the server's activity feed; one tab per branch
//!   running `by log --follow`, with the branch's state reported on it.
//! - `log`: the command in a branch tab (`BRANCHYARD_HERDR_BRANCH`).
//! - `start`: open the bridge in a tab.
//! - `action merge|cancel|send`: act on the branch in the focused pane.
//! - `send`: the popup that reads a prompt and runs `by send`.

mod actions;
mod bridge;
mod config;
mod herdr;
mod model;

use std::io::IsTerminal;
use std::process::ExitCode;

use config::Config;

/// The plugin's command line; Herdr runs it from `herdr-plugin.toml`.
#[derive(clap::Parser, Debug)]
#[command(
    name = "branchyard-herdr",
    version,
    about = "A Herdr plugin that shows a Branchyard server's branches as Herdr panes.",
    after_help = "Settings not given as flags come from BRANCHYARD_REMOTE, BRANCHYARD_TOKEN_FILE,\n\
                  BRANCHYARD_REPO and BRANCHYARD_CA_FILE, then from config.env in the plugin's\n\
                  Herdr config directory."
)]
struct Cli {
    /// The Branchyard server
    #[arg(long, global = true, value_name = "URL")]
    remote: Option<String>,
    /// The server's bearer token, on the first line of FILE
    #[arg(long, global = true, value_name = "FILE")]
    token_file: Option<String>,
    /// Repository on the server, when it serves several
    #[arg(long, global = true, value_name = "NAME")]
    repo: Option<String>,
    /// Also trust this CA certificate for https
    #[arg(long, global = true, value_name = "FILE")]
    ca_file: Option<String>,
    /// Log lines on stderr as pretty text or one JSON object each (default:
    /// BRANCHYARD_LOG_FORMAT, else pretty)
    #[arg(long, global = true, value_name = "FORMAT", value_enum)]
    log_format: Option<LogFormat>,
    #[command(subcommand)]
    command: Plugin,
}

/// How log lines are written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum LogFormat {
    /// Human-readable lines.
    Pretty,
    /// One JSON object per line.
    Json,
}

impl LogFormat {
    /// The flag's format if given, else `BRANCHYARD_LOG_FORMAT`'s (`json`
    /// or `pretty`, any case; anything else is `pretty`), else `pretty`.
    fn resolve(flag: Option<LogFormat>, env: Option<&str>) -> LogFormat {
        flag.unwrap_or(match env {
            Some(value) if value.trim().eq_ignore_ascii_case("json") => LogFormat::Json,
            _ => LogFormat::Pretty,
        })
    }
}

#[derive(clap::Subcommand, Debug, PartialEq, Eq)]
enum Plugin {
    /// Follow the server and keep one Herdr pane per branch
    Bridge,
    /// Open the bridge in a new Herdr tab
    Start,
    /// Run `by log --follow $BRANCHYARD_HERDR_BRANCH` (a branch pane)
    Log,
    /// Act on the branch shown in the focused pane
    Action {
        #[arg(value_parser = ["merge", "cancel", "send"])]
        name: String,
    },
    /// Read a prompt and run `by send $BRANCHYARD_HERDR_BRANCH` (a popup)
    Send,
}

/// Sets up the process's tracing subscriber: level from `BRANCHYARD_LOG` or
/// `RUST_LOG` (default `info`), format from `--log-format`, else
/// `BRANCHYARD_LOG_FORMAT`, else `pretty` ([`LogFormat::resolve`]). Writes
/// to stderr, same as the rest of this plugin's diagnostics, so a Herdr
/// pane still shows them.
#[allow(clippy::let_underscore_must_use)] // ratchet: branchyard-herdr
fn init_logging(format: Option<LogFormat>) {
    use tracing_subscriber::EnvFilter;
    let filter = std::env::var("BRANCHYARD_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .ok()
        .and_then(|directives| EnvFilter::try_new(directives).ok())
        .unwrap_or_else(|| EnvFilter::new("info"));
    let env = std::env::var("BRANCHYARD_LOG_FORMAT").ok();
    let json = LogFormat::resolve(format, env.as_deref()) == LogFormat::Json;
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr);
    let _ = if json {
        builder.json().try_init()
    } else {
        builder.try_init()
    };
}

#[allow(clippy::let_underscore_must_use)] // ratchet: branchyard-herdr
fn main() -> ExitCode {
    let cli = <Cli as clap::Parser>::parse();
    init_logging(cli.log_format);
    let flags: Vec<(String, String)> = [
        ("BRANCHYARD_REMOTE", cli.remote),
        ("BRANCHYARD_TOKEN_FILE", cli.token_file),
        ("BRANCHYARD_REPO", cli.repo),
        ("BRANCHYARD_CA_FILE", cli.ca_file),
    ]
    .into_iter()
    .filter_map(|(name, value)| Some((name.to_owned(), value?)))
    .collect();
    let config = Config::load(&flags);
    let result = match &cli.command {
        Plugin::Bridge => bridge::run(&config),
        Plugin::Start => actions::start(&config),
        Plugin::Log => actions::log(&config),
        Plugin::Send => actions::send_popup(&config),
        Plugin::Action { name } => actions::action(&config, name),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("branchyard-herdr: {message}");
            // In a Herdr pane, which closes when this exits: keep the
            // reason on screen.
            if cli.command == Plugin::Bridge && std::io::stdin().is_terminal() {
                eprint!("press Enter to close ");
                let _ = std::io::stdin().read_line(&mut String::new());
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn log_format_is_a_flag_that_wins_over_the_variable() {
        let cli =
            Cli::try_parse_from(["branchyard-herdr", "bridge", "--log-format", "json"]).unwrap();
        assert_eq!(cli.log_format, Some(LogFormat::Json));
        let cli = Cli::try_parse_from(["branchyard-herdr", "bridge"]).unwrap();
        assert_eq!(cli.log_format, None);
        assert!(
            Cli::try_parse_from(["branchyard-herdr", "--log-format", "xml", "bridge"]).is_err()
        );
        assert_eq!(LogFormat::resolve(None, Some("json")), LogFormat::Json);
        assert_eq!(
            LogFormat::resolve(Some(LogFormat::Pretty), Some("json")),
            LogFormat::Pretty
        );
        assert_eq!(LogFormat::resolve(None, None), LogFormat::Pretty);
    }
}
