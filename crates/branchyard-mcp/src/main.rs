//! `branchyard-mcp`: Branchyard's delegation tools for one branch, over MCP
//! on stdio. See the library documentation.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match branchyard_mcp::main_with_args(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(branchyard_mcp::Failure::Usage(message)) => {
            eprintln!("branchyard-mcp: {message}");
            eprint!("{}", branchyard_mcp::USAGE);
            ExitCode::from(2)
        }
        Err(branchyard_mcp::Failure::Serve(message)) => {
            eprintln!("branchyard-mcp: {message}");
            ExitCode::FAILURE
        }
    }
}
