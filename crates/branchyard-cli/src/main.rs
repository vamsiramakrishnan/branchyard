use branchyard_sdk::{protocol::*, Client, ClientOptions, Error};
use clap::{Parser, Subcommand};
use serde::Serialize;
use serde_json::{json, Value};
use std::io::{self, Read};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(
    version,
    about = "Remote control client for Branchyard; never launches a local harness"
)]
struct Cli {
    #[arg(long, env = "BRANCHYARD_ENDPOINT", global = true)]
    endpoint: Option<String>,
    #[arg(long, default_value_t = 30, global = true)]
    timeout_seconds: u64,
    #[arg(
        long,
        global = true,
        help = "Allow HTTP to a literal loopback IP for a fixture or tunnel"
    )]
    allow_loopback_http: bool,
    #[command(subcommand)]
    command: Commands,
}
#[derive(Subcommand)]
enum Commands {
    /// Print generated schemas and bounds without connecting to a server.
    Describe,
    /// Generate an ID to persist in a command before submitting it.
    NewId,
    /// Validate a command's shape and bounds locally (not authorization or graph validity).
    Validate {
        #[arg(long)]
        file: PathBuf,
    },
    /// Submit a saved command once. Timeout may mean accepted; reconcile the same file.
    Call {
        #[arg(long)]
        file: PathBuf,
    },
    /// Look up the saved command's operation and verify its input fingerprint.
    Reconcile {
        #[arg(long)]
        file: PathBuf,
    },
    /// Query server readiness and qualified profiles. Does not provision anything.
    Doctor,
    /// Inspect one remote task.
    Task { id: TaskId },
    /// Inspect one remote operation. Prefer reconcile when you have the original file.
    Operation { id: OperationId },
    /// Read one bounded event page; retain next_after for subsequent requests.
    Events {
        id: TaskId,
        #[arg(long, default_value_t = 0)]
        after: u64,
        #[arg(long, default_value_t = 50)]
        limit: u16,
    },
}
fn command_file(path: &PathBuf) -> Result<Command, Error> {
    let reader: Box<dyn Read> = if path.as_os_str() == "-" {
        Box::new(io::stdin())
    } else {
        Box::new(
            std::fs::File::open(path)
                .map_err(|_| Error::Configuration("cannot open command file"))?,
        )
    };
    let mut bytes = Vec::new();
    reader
        .take((MAX_REQUEST_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::Configuration("cannot read command file"))?;
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(Invalid("command file exceeds 256 KiB").into());
    }
    let command: Command = serde_json::from_slice(&bytes)
        .map_err(|_| Invalid("invalid command JSON; use describe for its schema"))?;
    command.canonical_bytes()?;
    Ok(command)
}
fn value(input: impl Serialize) -> Value {
    serde_json::to_value(input).expect("serializable protocol type")
}
async fn run(cli: Cli) -> Result<Value, Error> {
    match &cli.command {
        Commands::Describe => return Ok(schema()),
        Commands::NewId => return Ok(json!({"id": uuid::Uuid::new_v4()})),
        Commands::Validate { file } => {
            let c = command_file(file)?;
            return Ok(
                json!({"valid": true, "scope": "local_shape_and_bounds", "operation_id": c.operation_id, "request_sha256": c.fingerprint()?}),
            );
        }
        _ => {}
    }
    let endpoint = cli.endpoint.as_deref().ok_or(Error::Configuration(
        "set BRANCHYARD_ENDPOINT or --endpoint",
    ))?;
    let token = std::env::var("BRANCHYARD_TOKEN").map_err(|_| {
        Error::Configuration("set BRANCHYARD_TOKEN through your credential mechanism")
    })?;
    let client = Client::with_options(
        endpoint,
        &token,
        ClientOptions {
            timeout: Duration::from_secs(cli.timeout_seconds),
            allow_loopback_http: cli.allow_loopback_http,
        },
    )?;
    match cli.command {
        Commands::Call { file } => Ok(value(client.submit(&command_file(&file)?).await?)),
        Commands::Reconcile { file } => Ok(value(client.reconcile(&command_file(&file)?).await?)),
        Commands::Doctor => Ok(value(client.info().await?)),
        Commands::Task { id } => Ok(value(client.task(id).await?)),
        Commands::Operation { id } => Ok(value(client.operation(id).await?)),
        Commands::Events { id, after, limit } => Ok(value(client.events(id, after, limit).await?)),
        _ => unreachable!(),
    }
}
#[tokio::main]
async fn main() {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            let _ = error.print();
            return;
        }
        Err(_) => {
            eprintln!(
                "{}",
                json!({"error": "invalid_arguments", "message": "use branchyard --help"})
            );
            std::process::exit(2);
        }
    };
    match run(cli).await {
        Ok(output) => println!("{output}"),
        Err(error) => {
            let (code, name) = match &error {
                Error::Invalid(_) | Error::Configuration(_) => (2, "invalid_request"),
                Error::SubmissionUnknown { .. } => (4, "submission_unknown"),
                Error::Protocol | Error::ResponseTooLarge => (5, "invalid_response"),
                _ => (3, "remote_error"),
            };
            let mut output = json!({"error": name, "message": error.to_string()});
            if let Error::Http { status } = &error {
                output["status"] = value(status);
            }
            if let Error::SubmissionUnknown {
                operation_id,
                request_sha256,
            } = error
            {
                output["operation_id"] = value(operation_id);
                output["request_sha256"] = value(request_sha256);
            }
            eprintln!("{output}");
            std::process::exit(code);
        }
    }
}
