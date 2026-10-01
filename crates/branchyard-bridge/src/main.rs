//! `branchyard-bridge`: serve the bridge inside a sandbox, or generate the
//! host's signing key.

use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use clap::{ArgAction, Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use std::process::ExitCode;

use branchyard_bridge::credential::{Signer, Verifier, KEY_ENV};
use branchyard_bridge::server::{self, Bridge, Config};
use branchyard_bridge::ServerTls;

/// The bridge's command line.
#[derive(Parser, Debug)]
#[command(
    name = "branchyard-bridge",
    about = "Serve the Branchyard bridge inside a sandbox, or generate the host's signing key.",
    disable_version_flag = true,
    subcommand_required = true
)]
struct Cli {
    /// Print the version and the bridge protocol's
    #[arg(long, action = ArgAction::Version)]
    version: (),
    #[command(subcommand)]
    command: Bridged,
}

#[derive(Subcommand, Debug)]
enum Bridged {
    /// Serve execs and file transfers for the host
    #[command(long_about = SERVE_ABOUT)]
    Serve(ServeArgs),
    /// Write a new signing key and print its public key
    #[command(
        long_about = "Write a new signing key to FILE (mode 0600, never replacing a file) and \
                      print its public key, the value for $BRANCHYARD_BRIDGE_KEY."
    )]
    Keygen {
        /// Where to write the key
        #[arg(long, value_name = "FILE")]
        out: PathBuf,
    },
}

const SERVE_ABOUT: &str = "\
Serve execs and file transfers for the host.

The verifying key is read from $BRANCHYARD_BRIDGE_KEY. Prints `listening ADDR`
once bound. SIGTERM is forwarded to the execs, which are killed after 10
seconds; once their last output and exit statuses are sent (up to 2 seconds
more), the bridge exits.";

#[derive(Args, Debug)]
struct ServeArgs {
    /// Address to listen on
    #[arg(long, value_name = "ADDR", default_value = "0.0.0.0:8080")]
    listen: SocketAddr,
    /// The actor identity: DIR/atespace, DIR/name and DIR/uid
    #[arg(long, value_name = "DIR", default_value = "/run/branchyard/identity")]
    identity: PathBuf,
    /// Where attempt state is kept
    #[arg(
        long,
        value_name = "FILE",
        default_value = "/var/lib/branchyard-bridge/attempts"
    )]
    state: PathBuf,
    /// Serve TLS with this PEM certificate chain
    #[arg(long, value_name = "FILE", requires = "tls_key")]
    tls_cert: Option<PathBuf>,
    /// The PEM key of --tls-cert
    #[arg(long, value_name = "FILE", requires = "tls_cert")]
    tls_key: Option<PathBuf>,
    /// Run execs and move files as this user and group (the bridge must be root)
    #[arg(long, value_name = "UID:GID", value_parser = uid_gid)]
    run_as: Option<(u32, u32)>,
    /// Exit when stdin closes
    #[arg(long)]
    lifeline_stdin: bool,
}

fn uid_gid(text: &str) -> Result<(u32, u32), String> {
    text.split_once(':')
        .and_then(|(u, g)| Some((u.parse().ok()?, g.parse().ok()?)))
        .ok_or_else(|| "needs numeric UID:GID such as 1000:1000".into())
}

fn main() -> ExitCode {
    let version = format!(
        "{} (protocol {})",
        env!("CARGO_PKG_VERSION"),
        branchyard_bridge::protocol::VERSION
    );
    // Once per process; clap takes a `'static` version.
    let matches = Cli::command().version(&*version.leak()).get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(error) => error.exit(),
    };
    let result = match cli.command {
        Bridged::Serve(args) => serve(args),
        Bridged::Keygen { out } => keygen(&out),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("branchyard-bridge: {message}");
            ExitCode::FAILURE
        }
    }
}

fn serve(args: ServeArgs) -> Result<(), String> {
    // Before any thread starts, so every thread leaves these signals to
    // the bridge's supervisor.
    server::block_signals();
    let key = std::env::var(KEY_ENV).map_err(|_| format!("{KEY_ENV} is not set"))?;
    let verifier = Verifier::from_hex(&key).map_err(|e| e.to_string())?;
    let tls = match (args.tls_cert, args.tls_key) {
        (Some(cert), Some(key)) => {
            Some(ServerTls::from_pem_files(&cert, &key).map_err(|e| e.to_string())?)
        }
        _ => None,
    };
    let bridge = Bridge::bind(Config {
        listen: args.listen,
        verifier,
        identity_dir: args.identity,
        state_file: args.state,
        lifeline: args.lifeline_stdin,
        tls,
        run_as: args.run_as,
    })
    .map_err(|e| format!("could not start: {e}"))?;
    let address = bridge.local_addr().map_err(|e| e.to_string())?;
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "listening {address}");
    let _ = stdout.flush();
    drop(stdout);
    bridge.serve().map_err(|e| e.to_string())
}

fn keygen(out: &Path) -> Result<(), String> {
    let signer =
        Signer::write(out).map_err(|e| format!("could not write {}: {e}", out.display()))?;
    println!("{}", signer.public_key());
    Ok(())
}
