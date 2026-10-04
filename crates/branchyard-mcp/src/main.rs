//! `branchyard-mcp`: Branchyard's delegation tools for one branch, over MCP
//! on stdio. See the library documentation.

use std::process::ExitCode;

#[allow(clippy::let_underscore_must_use)] // ratchet: branchyard-mcp
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match branchyard_mcp::main_with_args(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(branchyard_mcp::Failure::Clap(error)) => {
            let _ = error.print();
            ExitCode::from(error.exit_code() as u8)
        }
        Err(branchyard_mcp::Failure::Usage(message)) => {
            eprintln!("branchyard-mcp: {message}");
            ExitCode::from(2)
        }
        Err(branchyard_mcp::Failure::Serve(message)) => {
            eprintln!("branchyard-mcp: {message}");
            ExitCode::FAILURE
        }
    }
}
