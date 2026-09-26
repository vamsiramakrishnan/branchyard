//! `by`: delegate coding work to agent harnesses on git branches, and merge
//! only validated results. A thin client of the `branchyard` SDK in local
//! mode.

mod args;
mod commands;
mod console;
mod json;
mod render;

use std::io;
use std::process::ExitCode;

use args::Command;
use commands::{Env, Failure};

fn main() -> ExitCode {
    let argv: Result<Vec<String>, _> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.into_string())
        .collect();
    let parsed = match argv {
        Ok(argv) => args::parse(&argv),
        Err(_) => Err(args::UsageError {
            message: "arguments must be valid UTF-8".into(),
            command: None,
        }),
    };
    let command = match parsed {
        Ok(command) => command,
        Err(error) => {
            eprintln!("by: {error}");
            match error.command {
                Some(name) => eprintln!("Try 'by help {name}'."),
                None => eprintln!("Try 'by help'."),
            }
            return ExitCode::from(2);
        }
    };
    match dispatch(&Env::detect(), command) {
        Ok(()) => ExitCode::SUCCESS,
        // A closed pipe, as in `by ls | head`, is the reader's choice.
        Err(Failure::Io(error)) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(Failure::Reported) => ExitCode::FAILURE,
        Err(failure) => {
            eprintln!("by: {failure}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(env: &Env, command: Command) -> commands::Outcome {
    match command {
        Command::Run { prompt, task } => commands::run(env, &prompt, &task),
        Command::Fan {
            prompt,
            harnesses,
            task,
        } => commands::fan(env, &prompt, &harnesses, &task),
        Command::Send {
            branch,
            prompt,
            task,
        } => commands::send(env, &branch, &prompt, &task),
        Command::Fork {
            branch,
            prompt,
            fresh_session,
            task,
        } => commands::fork(env, &branch, &prompt, fresh_session, &task),
        Command::Ls { json } => commands::ls(env, json),
        Command::Show { branch, json } => commands::show(env, &branch, json),
        Command::Diff { branch } => commands::diff(env, &branch),
        Command::Log { branch, json } => commands::log(env, &branch, json),
        Command::Merge { branch, into } => commands::merge(&branch, into.as_deref()),
        Command::Rm { branch } => commands::rm(&branch),
        Command::Harnesses { json } => commands::harnesses(env, json),
        Command::Help { topic: None } => commands::print(&args::general_help()),
        Command::Help { topic: Some(spec) } => commands::print(&args::command_help(spec)),
        Command::Version => commands::print(&format!("by {}\n", env!("CARGO_PKG_VERSION"))),
    }
}
