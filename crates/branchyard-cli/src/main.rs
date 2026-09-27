//! `by`: delegate coding work to agent harnesses on git branches, and merge
//! only validated results. A thin client of the `branchyard` SDK in local
//! mode, and of a Branchyard server through `branchyard-client` in remote
//! mode (`--remote URL`).

mod args;
mod commands;
mod console;
mod json;
mod remote;
mod render;
mod rig;
mod watch;

use std::io;
use std::process::ExitCode;

use args::{Command, Globals};
use commands::{Env, Failure, Target};

fn main() -> ExitCode {
    let argv: Result<Vec<String>, _> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.into_string())
        .collect();
    let parsed = match &argv {
        Ok(argv) => {
            args::parse_globals(argv).and_then(|(globals, rest)| Ok((globals, args::parse(rest)?)))
        }
        Err(_) => Err(args::UsageError {
            message: "arguments must be valid UTF-8".into(),
            command: None,
        }),
    };
    let (globals, command) = match parsed {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("by: {error}");
            match error.command {
                Some(name) => eprintln!("Try 'by help {name}'."),
                None => eprintln!("Try 'by help'."),
            }
            return ExitCode::from(2);
        }
    };
    let globals = globals.with_env(|name| std::env::var(name).ok());
    if let Command::Serve { args } = &command {
        if globals.remote.is_some() {
            eprintln!(
                "by: serve runs a server here; it does not take --remote or BRANCHYARD_REMOTE"
            );
            return ExitCode::from(2);
        }
        return branchyard_server::cli::main(args, "by serve");
    }
    match run(&Env::detect(), &globals, command) {
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

/// Commands that need no repository or server run first; the rest run
/// where `globals` point.
fn run(env: &Env, globals: &Globals, command: Command) -> commands::Outcome {
    match command {
        Command::Help { topic: None } => return commands::print(&args::general_help()),
        Command::Help { topic: Some(spec) } if spec.name == "serve" => {
            return commands::print(branchyard_server::cli::USAGE)
        }
        Command::Help { topic: Some(spec) } => return commands::print(&args::command_help(spec)),
        Command::Version => return commands::print(&format!("by {}\n", env!("CARGO_PKG_VERSION"))),
        // Started by the engine for one branch, always beside it.
        Command::Mcp { args } => return commands::mcp(&args),
        _ => {}
    }
    let target = match &globals.remote {
        Some(_) => Target::Remote(Box::new(remote::Remote::connect(globals)?)),
        None if globals.token_file.is_some() || globals.repo.is_some() => {
            return Err(Failure::Message(
                "--token-file and --repo apply to remote mode; pass --remote URL too".into(),
            ))
        }
        None => Target::Local,
    };
    dispatch(env, &target, command)
}

fn dispatch(env: &Env, target: &Target, command: Command) -> commands::Outcome {
    match command {
        Command::Run { prompt, task } => commands::run(env, target, &prompt, &task),
        Command::Fan {
            prompt,
            harnesses,
            task,
        } => commands::fan(env, target, &prompt, &harnesses, &task),
        Command::Send {
            branch,
            prompt,
            task,
            json,
        } => commands::send(env, target, &branch, &prompt, &task, json),
        Command::Fork {
            branch,
            prompt,
            fresh_session,
            task,
        } => commands::fork(env, target, &branch, &prompt, fresh_session, &task),
        Command::Ls { json } => commands::ls(env, target, json),
        Command::Show { branch, json } => commands::show(env, target, &branch, json),
        Command::Diff { branch } => commands::diff(env, target, &branch),
        Command::Log { branch, json } => commands::log(env, target, &branch, json),
        Command::Merge { branch, into } => commands::merge(target, &branch, into.as_deref()),
        Command::Rm {
            branch,
            keep_credentials,
        } => commands::rm(target, &branch, keep_credentials),
        Command::Harnesses { json } => commands::harnesses(env, target, json),
        Command::Watch { interval, once } => watch::run(env, target, interval, once),
        Command::Cancel { branch, json } => commands::cancel(target, &branch, json),
        Command::Spawn { prompt, spawn } => commands::spawn(env, target, &prompt, &spawn),
        Command::Inspect { branch, json } => commands::inspect(env, target, branch, json),
        Command::Events {
            branch,
            cursor,
            limit,
            json,
        } => commands::events(env, target, branch, cursor, limit, json),
        Command::Integrate { branch, json } => commands::integrate(target, &branch, json),
        Command::Children { branch, json } => commands::children(env, target, branch, json),
        Command::Rig(args) => commands::rig(env, target, &args),
        Command::Help { .. } | Command::Version | Command::Serve { .. } | Command::Mcp { .. } => {
            unreachable!("handled before choosing a target")
        }
    }
}
