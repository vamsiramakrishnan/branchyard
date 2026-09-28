//! `by`: delegate coding work to agent harnesses on git branches, and merge
//! only validated results. A thin client of the `branchyard` SDK in local
//! mode, and of a Branchyard server through `branchyard-client` in remote
//! mode (`--remote URL`).

mod args;
mod attempts;
mod commands;
mod config_cmd;
mod console;
mod defaults;
mod gh;
mod init;
mod json;
mod open;
mod pr;
mod notify;
mod remote;
mod render;
mod rig;
mod setup_io;
mod watch;
mod wizard;

use std::ffi::OsString;
use std::io;
use std::process::ExitCode;

use args::{Command, Globals};
use commands::{Env, Failure, Target};

fn main() -> ExitCode {
    let argv: Vec<OsString> = std::env::args_os().collect();
    args::unset_blank_env();
    if let Some(call) = args::server_call(&argv[1..]) {
        return serve(&argv[..=call.prefix], call);
    }
    let cli = match args::parse_from(&argv) {
        Ok(cli) => cli,
        // Help and --version come this way too, to stdout with exit 0;
        // usage errors go to stderr with exit 2.
        Err(error) => {
            let _ = error.print();
            return ExitCode::from(error.exit_code() as u8);
        }
    };
    let Some(command) = cli.command else {
        let _ = args::command().print_help();
        return ExitCode::SUCCESS;
    };
    // branchyard.toml and the user configuration, under flags and variables.
    let cwd = std::env::current_dir().unwrap_or_default();
    let env = |name: &str| std::env::var(name).ok();
    let (globals, command) = match defaults::apply(&cwd, &env, cli.globals, command) {
        Ok(applied) => applied,
        Err(error) => {
            eprintln!("by: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut env_now = Env::detect();
    env_now.notify = notify::Settings::resolve(globals.no_notify, &globals.notify, &env);
    match run(&env_now, &globals, command) {
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

/// `by serve`, `by worker` and their help: the server's own command line.
/// `prefix` is `by`'s part, parsed only for its global options.
fn serve(prefix: &[OsString], call: args::ServerCall) -> ExitCode {
    let globals = match args::parse_from(prefix) {
        Ok(cli) => cli.globals,
        Err(error) => {
            let _ = error.print();
            return ExitCode::from(error.exit_code() as u8);
        }
    };
    if call.help {
        return match commands::print(&branchyard_server::cli::help(call.program)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::FAILURE,
        };
    }
    if globals.remote.is_some() {
        eprintln!("by: serve runs a server here; it does not take --remote or BRANCHYARD_REMOTE");
        return ExitCode::from(2);
    }
    // `[serve] config` from branchyard.toml, unless the arguments name one.
    let cwd = std::env::current_dir().unwrap_or_default();
    let env = |name: &str| std::env::var(name).ok();
    let args = match defaults::apply_serve(&cwd, &env, call.args) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("by: {error}");
            return ExitCode::FAILURE;
        }
    };
    branchyard_server::cli::main(&args, call.program)
}

/// Commands that need no repository or server run first; the rest run
/// where `globals` point.
fn run(env: &Env, globals: &Globals, command: Command) -> commands::Outcome {
    match command {
        // Started by the engine for one branch, always beside it.
        Command::Mcp { root, branch } => return commands::mcp(&root, &branch),
        Command::Completions { shell } => {
            let mut out = Vec::new();
            clap_complete::generate(shell, &mut args::command(), "by", &mut out);
            return commands::print(&String::from_utf8_lossy(&out));
        }
        Command::Man => {
            let mut out = Vec::new();
            clap_mangen::Man::new(args::command()).render(&mut out)?;
            return commands::print(&String::from_utf8_lossy(&out));
        }
        // Setup needs no repository or server: it may be what creates them.
        Command::Init { init } => return init::main(env, &init),
        Command::Config { json, action } => return config_cmd::main(&action, json),
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

/// One arm per command that runs against a repository or a server; see
/// "Adding a command" in `args.rs`.
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
            steer: true,
            wait: _,
            json,
        } => commands::steer(target, &branch, &prompt, &task, json),
        Command::Send {
            branch,
            prompt,
            task,
            steer: false,
            wait,
            json,
        } => commands::send(env, target, &branch, &prompt, &task, wait, json),
        Command::Fork {
            branch,
            prompt,
            fresh_session,
            at: None,
            task,
        } => commands::fork(env, target, &branch, &prompt, fresh_session, &task),
        Command::Fork {
            branch,
            prompt,
            at: Some(turn),
            task,
            ..
        } => commands::fork_at(env, target, &branch, turn, &prompt, &task),
        Command::Rewind {
            branch,
            to,
            yes,
            json,
        } => attempts::rewind(env, target, &branch, to, yes, json),
        Command::Try {
            branch,
            off,
            status,
            force,
            json,
        } => attempts::try_branch(env, target, branch.as_deref(), off, status, force, json),
        Command::Compare {
            branches,
            fan,
            check,
            diff,
            pick,
            into,
            discard_others,
            yes,
            json,
        } => attempts::compare(
            env,
            target,
            &attempts::CompareArgs {
                branches,
                fan,
                check,
                diff,
                pick,
                into,
                discard_others,
                yes,
                json,
            },
        ),
        Command::Reincarnate { branch, task } => commands::reincarnate(env, target, &branch, &task),
        Command::Ls { json } => commands::ls(env, target, json),
        Command::Show {
            branch,
            json,
            refresh,
        } => commands::show(env, target, &branch, json, refresh),
        Command::Diff { branch } => commands::diff(env, target, &branch),
        Command::Log {
            branch,
            json,
            follow,
        } => commands::log(env, target, &branch, json, follow),
        Command::Merge { branch, into } => commands::merge(target, &branch, into.as_deref()),
        Command::Rm {
            branch,
            keep_credentials,
        } => commands::rm(target, &branch, keep_credentials),
        Command::Harnesses { json } => commands::harnesses(env, target, json),
        Command::Pr { branch, pr } => pr::main(env, target, &branch, &pr),
        Command::Open {
            branch,
            editor,
            print,
        } => open::main(target, &branch, editor.as_deref(), print),
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
        Command::Graph { json, action } => commands::graph(env, target, &action.into_args(json)),
        Command::Ask {
            as_branch,
            text,
            wait_seconds,
            json,
        } => commands::ask(target, as_branch, &text, wait_seconds, json),
        Command::Report {
            as_branch,
            text,
            json,
        } => commands::report(target, as_branch, &text, json),
        Command::Escalate {
            as_branch,
            text,
            json,
        } => commands::escalate(target, as_branch, &text, json),
        Command::Answer {
            as_branch,
            message_id,
            text,
            json,
        } => commands::answer(target, as_branch, message_id, &text, json),
        Command::Inbox {
            as_branch,
            unread,
            json,
        } => commands::inbox(target, as_branch, unread, json),
        Command::Rig { json, action } => commands::rig(env, target, &action.into_args(json)),
        Command::Artifact {
            branch,
            json,
            action,
        } => commands::artifact(target, &action.into_args(branch, json)),
        Command::Scratch {
            branch,
            json,
            action,
        } => commands::scratch(target, &action.into_args(branch, json)),
        Command::Serve { .. }
        | Command::Worker { .. }
        | Command::Mcp { .. }
        | Command::Completions { .. }
        | Command::Man
        | Command::Init { .. }
        | Command::Config { .. } => unreachable!("handled before choosing a target"),
    }
}
