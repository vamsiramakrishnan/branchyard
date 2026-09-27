//! `branchyard-bridge`: serve the bridge inside a sandbox, or generate the
//! host's signing key.

use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use branchyard_bridge::credential::{Signer, Verifier, KEY_ENV};
use branchyard_bridge::server::{self, Bridge, Config};
use branchyard_bridge::ServerTls;

const USAGE: &str = "\
usage:
  branchyard-bridge serve [--listen ADDR] [--identity DIR] [--state FILE]
                         [--tls-cert FILE --tls-key FILE] [--run-as UID:GID]
                         [--lifeline-stdin]
      Serve on ADDR (default 0.0.0.0:8080). The verifying key is read from
      $BRANCHYARD_BRIDGE_KEY; the actor identity from DIR/atespace, DIR/name
      and DIR/uid (default /run/branchyard/identity); attempt state is kept in
      FILE (default /var/lib/branchyard-bridge/attempts). With --tls-cert and
      --tls-key (PEM chain and key), serve TLS. With --run-as, run execs and
      move files as that user and group (the bridge must be root). Prints
      `listening ADDR` once bound. SIGTERM is forwarded to the execs, which
      are killed after 10 seconds; then the bridge exits.
  branchyard-bridge keygen --out FILE
      Write a new signing key to FILE (mode 0600, never replacing a file) and
      print its public key, the value for $BRANCHYARD_BRIDGE_KEY.
  branchyard-bridge --version";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("branchyard-bridge: {message}");
            ExitCode::FAILURE
        }
    }
}

fn value<'a>(args: &'a [String], at: &mut usize, flag: &str) -> Result<&'a str, String> {
    *at += 1;
    args.get(*at)
        .map(String::as_str)
        .ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))
}

fn run(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("serve") => serve(&args[1..]),
        Some("keygen") => keygen(&args[1..]),
        Some("--version") => {
            println!(
                "branchyard-bridge {} (protocol {})",
                env!("CARGO_PKG_VERSION"),
                branchyard_bridge::protocol::VERSION
            );
            Ok(())
        }
        _ => Err(USAGE.into()),
    }
}

fn serve(args: &[String]) -> Result<(), String> {
    // Before any thread starts, so every thread leaves these signals to
    // the bridge's supervisor.
    server::block_signals();
    let mut tls_cert: Option<PathBuf> = None;
    let mut tls_key: Option<PathBuf> = None;
    let mut run_as = None;
    let mut listen: SocketAddr = "0.0.0.0:8080".parse().unwrap();
    let mut identity_dir = PathBuf::from("/run/branchyard/identity");
    let mut state_file = PathBuf::from("/var/lib/branchyard-bridge/attempts");
    let mut lifeline = false;
    let mut at = 0;
    while at < args.len() {
        match args[at].as_str() {
            "--listen" => {
                let text = value(args, &mut at, "--listen")?;
                listen = text.parse().map_err(|_| {
                    format!("--listen needs an address such as 0.0.0.0:8080, not {text}")
                })?;
            }
            "--identity" => identity_dir = value(args, &mut at, "--identity")?.into(),
            "--state" => state_file = value(args, &mut at, "--state")?.into(),
            "--lifeline-stdin" => lifeline = true,
            "--tls-cert" => tls_cert = Some(value(args, &mut at, "--tls-cert")?.into()),
            "--tls-key" => tls_key = Some(value(args, &mut at, "--tls-key")?.into()),
            "--run-as" => {
                let text = value(args, &mut at, "--run-as")?;
                let parsed = text
                    .split_once(':')
                    .and_then(|(u, g)| Some((u.parse().ok()?, g.parse().ok()?)));
                run_as = Some(parsed.ok_or_else(|| {
                    format!("--run-as needs numeric UID:GID such as 1000:1000, not {text}")
                })?);
            }
            other => return Err(format!("unknown argument {other}\n{USAGE}")),
        }
        at += 1;
    }
    let key = std::env::var(KEY_ENV).map_err(|_| format!("{KEY_ENV} is not set"))?;
    let verifier = Verifier::from_hex(&key).map_err(|e| e.to_string())?;
    let tls = match (tls_cert, tls_key) {
        (None, None) => None,
        (Some(cert), Some(key)) => {
            Some(ServerTls::from_pem_files(&cert, &key).map_err(|e| e.to_string())?)
        }
        _ => return Err("--tls-cert and --tls-key go together".into()),
    };
    let bridge = Bridge::bind(Config {
        listen,
        verifier,
        identity_dir,
        state_file,
        lifeline,
        tls,
        run_as,
    })
    .map_err(|e| format!("could not start: {e}"))?;
    let address = bridge.local_addr().map_err(|e| e.to_string())?;
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "listening {address}");
    let _ = stdout.flush();
    drop(stdout);
    bridge.serve().map_err(|e| e.to_string())
}

fn keygen(args: &[String]) -> Result<(), String> {
    let out = match args {
        [flag, path] if flag == "--out" => PathBuf::from(path),
        _ => return Err(USAGE.into()),
    };
    let signer =
        Signer::write(&out).map_err(|e| format!("could not write {}: {e}", out.display()))?;
    println!("{}", signer.public_key());
    Ok(())
}
