use std::path::{Path, PathBuf};

use branchyard_client::Client;
use clap::Parser;

/// The credential environment variable used only by an external manager.
pub const TOKEN_ENV: &str = branchyard_support::MANAGER_TOKEN_ENV;

#[derive(Parser, Debug)]
#[command(
    name = "branchyard-mcp manager",
    about = "Manage authorized projects and workspaces over MCP on stdio."
)]
struct Cli {
    /// Branchyard server URL; no arbitrary URL is accepted by a tool.
    #[arg(long, value_name = "URL")]
    url: String,
    /// File whose first line contains the manager's scoped bearer token.
    #[arg(long, value_name = "FILE")]
    token_file: Option<PathBuf>,
    /// Additional PEM trust roots for a private server.
    #[arg(long, value_name = "FILE")]
    ca_file: Option<PathBuf>,
}

fn client(cli: Cli) -> Result<Client, crate::Failure> {
    let client = match cli.token_file {
        Some(path) => Client::from_token_file(&cli.url, path),
        None => {
            let token = std::env::var(TOKEN_ENV).map_err(|_| {
                crate::Failure::Usage(format!("pass --token-file or set {TOKEN_ENV}"))
            })?;
            Client::new(&cli.url, token)
        }
    }
    .map_err(|e| crate::Failure::Usage(e.to_string()))?;
    match cli.ca_file {
        Some(path) => client
            .with_ca_file(path)
            .map_err(|e| crate::Failure::Usage(e.to_string())),
        None => Ok(client),
    }
}

/// Parse and run the external manager command.
pub fn main_with_args(args: &[String]) -> Result<(), crate::Failure> {
    let cli = Cli::try_parse_from(
        std::iter::once("branchyard-mcp manager".to_owned()).chain(args.iter().cloned()),
    )
    .map_err(crate::Failure::Clap)?;
    serve_configured(&cli.url, cli.token_file.as_deref(), cli.ca_file.as_deref())
}

/// Serve an operator-configured endpoint, reading its credential only after the
/// managed-child authority guard. Paths are preserved without argument reparsing.
pub fn serve_configured(
    url: &str,
    token_file: Option<&Path>,
    ca_file: Option<&Path>,
) -> Result<(), crate::Failure> {
    super::ensure_operator().map_err(|e| crate::Failure::Usage(e.to_string()))?;
    let cli = Cli {
        url: url.to_owned(),
        token_file: token_file.map(Path::to_path_buf),
        ca_file: ca_file.map(Path::to_path_buf),
    };
    super::serve_stdio(client(cli)?)
        .map_err(|e| crate::Failure::Serve(format!("serving workspace manager: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard_testkit::Scratch;

    #[test]
    fn cli_has_no_plaintext_token_argument_and_requires_server_url() {
        assert!(Cli::try_parse_from([
            "manager",
            "--url",
            "http://localhost:8421",
            "--token",
            "secret"
        ])
        .is_err());
        assert!(Cli::try_parse_from(["manager", "--token-file", "token"]).is_err());
        let scratch = Scratch::new("manager-token");
        let path = scratch.path().join("token");
        std::fs::write(&path, "fixture-token\n").unwrap();
        let cli = Cli::try_parse_from([
            "manager",
            "--url",
            "http://localhost:8421",
            "--token-file",
            path.to_str().unwrap(),
        ])
        .unwrap();
        let configured = client(cli).unwrap();
        assert_eq!(configured.endpoint(), "http://localhost:8421");
        assert!(!format!("{configured:?}").contains("fixture-token"));
    }
}
