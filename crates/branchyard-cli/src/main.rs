//! `by`: delegate coding work to agent harnesses on git branches, and merge
//! only validated results. A thin client of the `branchyard` SDK in local
//! mode, and of a Branchyard server through `branchyard-client` in remote
//! mode (`--remote URL`).

mod adf;
mod adopt;
mod args;
mod attempts;
mod catalog_cmd;
mod commands;
mod config_cmd;
mod console;
mod defaults;
mod effects_cmd;
mod env_cmd;
mod fleet_cmd;
mod gateway_cmd;
mod gh;
mod harness_cmd;
mod init;
mod json;
mod knowledge_cmd;
mod live_catalog;
mod map_cmd;
mod models_cmd;
mod notify;
mod open;
mod plan_cmd;
mod ports;
mod pr;
mod pr_threads;
mod recipe_cmd;
mod remote;
mod render;
mod review;
mod review_format;
mod rig;
mod services_cmd;
mod setup_io;
mod ssh_remote;
mod stats_cmd;
mod sync_cmd;
mod task_cmd;
mod trackers;
mod trigger_cmd;
mod usage;
mod watch;
mod wizard;
mod workspace_cmd;

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
    // Warnings from best-effort steps (a cleanup that failed, a lock whose
    // holder panicked) go to stderr. Not under the live `watch` screen,
    // which owns the terminal.
    if !matches!(command, Command::Watch { once: false, .. }) {
        branchyard_server::logging::init_for_commands();
    }
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
    // A worker here advertises its harnesses with the usage meters `by
    // usage` reads (docs/harness-lifecycle.md).
    branchyard_server::ops::set_inventory_source(branchyard_server::ops::InventorySource(
        std::sync::Arc::new(harness_cmd::worker_inventory),
    ));
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
        // The meters read this machine's session files; no repository needed.
        Command::Usage { json } => return usage::show(env, json),
        // The ssh connection itself, not a server's API.
        Command::Remote { json, action } => return ssh_remote::command(globals, &action, json),
        // Live catalogs are cached for this user; no repository or server.
        Command::Catalog { json, action } => return catalog_cmd::live(&action, json),
        // Sync runs on this machine's repository, against its own remote.
        Command::Sync(sync) => {
            return sync_cmd::main(
                globals,
                sync.task.as_deref(),
                sync.action.as_ref(),
                sync.json,
            )
        }
        // The catalog is built in; no repository or server is involved.
        Command::Connectors {
            json,
            action: args::ConnectorsAction::Catalog,
        } => return catalog_cmd::connectors(env, json),
        _ => {}
    }
    if let (Some(_), Some(recipe)) = (&globals.remote, command.recipe()) {
        return Err(Failure::Message(format!(
            "--provider recipe:{recipe} runs on the machine that has the repository: a server \
             does not run environment recipes (docs/recipes.md). Run it without --remote"
        )));
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
            attempts,
            judge,
            task,
        } => commands::fan(
            env,
            target,
            &prompt,
            harnesses.as_ref().map(|h| h.0.as_slice()),
            &task,
            &commands::FanRoute { attempts, judge },
        ),
        Command::Map { json, action, map } => {
            map_cmd::main(env, target, action.as_ref(), &map, json)
        }
        Command::Judge {
            targets,
            harness,
            deterministic,
            command,
            rubric,
            pick,
            into,
            discard_others,
            yes,
            json,
        } => fleet_cmd::judge(
            env,
            target,
            &fleet_cmd::JudgeArgs {
                targets,
                harness,
                deterministic,
                command: command.map(|c| c.0),
                rubric,
                pick,
                into,
                discard_others,
                yes,
                json,
            },
            fleet_table()?.as_ref(),
        ),
        Command::Plan { json, action } => plan_cmd::main(env, target, &action, json),
        Command::Knowledge { json, action } => knowledge_cmd::main(env, target, &action, json),
        Command::Fleet { json, action } => match action {
            args::FleetAction::Stats { kind } => fleet_cmd::stats(env, target, kind, json),
            args::FleetAction::Route {
                prompt,
                kind,
                attempts,
                seed,
            } => fleet_cmd::route(
                target,
                &prompt,
                &branchyard::RouteOptions {
                    kind,
                    seed,
                    attempts,
                    failover: None,
                    ..Default::default()
                },
                fleet_table()?.as_ref(),
                json,
            ),
        },
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
        Command::Task(task) => task_cmd::main(env, target, &task.action, task.json),
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
        Command::Review {
            branch,
            print,
            editor,
            file,
            detach,
            task,
        } => review::main(
            env,
            target,
            &review::ReviewArgs {
                branch: &branch,
                print_only: print,
                editor: editor.as_deref(),
                file: file.as_deref(),
                detach,
                task: &task,
            },
        ),
        Command::Reincarnate { branch, task } => commands::reincarnate(env, target, &branch, &task),
        Command::Ls { json } => commands::ls(env, target, json),
        Command::Stats { json } => stats_cmd::main(env, target, json),
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
        Command::Merge {
            branch,
            into,
            rm,
            promote_effects,
        } => {
            if promote_effects {
                effects_cmd::promote_before_merge(target, &branch)?;
            }
            commands::merge(target, &branch, into.as_deref(), rm)
        }
        Command::Approvals(a) => effects_cmd::approvals(target, a.action.as_ref(), a.json),
        Command::Effects(e) => {
            effects_cmd::effects(target, e.branch.as_deref(), e.action.as_ref(), e.json)
        }
        Command::Undo(u) => effects_cmd::undo(env, target, &u),
        Command::Workspace { json, action } => workspace_cmd::main(env, target, &action, json),
        Command::Recipe { json, action } => recipe_cmd::main(env, target, &action, json),
        Command::Env { json, action } => env_cmd::main(env, target, &action, json),
        Command::Trigger { json, action } => trigger_cmd::main(env, target, &action, json),
        Command::Rm {
            branch,
            keep_credentials,
        } => commands::rm(target, &branch, keep_credentials),
        Command::Harnesses {
            json,
            all,
            profiles,
            refresh,
            on,
            action,
        } => harness_cmd::main(
            env,
            target,
            json,
            all,
            profiles,
            refresh,
            on.as_deref(),
            action.as_ref(),
        ),
        Command::Pr { branch, pr } => pr::main(env, target, &branch, &pr),
        Command::Open {
            branch,
            editor,
            print,
        } => open::main(target, &branch, editor.as_deref(), print),
        Command::Watch { interval, once } => watch::run(env, target, interval, once),
        Command::Adopt {
            session,
            list,
            name,
            no_diff,
            harness,
            json,
        } => adopt::main(
            env,
            target,
            &adopt::Asked {
                session: session.as_deref(),
                name: name.as_deref(),
                profile: harness.as_deref(),
                list,
                with_diff: !no_diff,
                json,
            },
        ),
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
        Command::Gateway { json, action } => gateway_cmd::main(target, &action, json),
        Command::Models { period, json } => models_cmd::main(env, target, &period, json),
        Command::Services {
            json,
            kind,
            all,
            action,
        } => services_cmd::main(env, target, action.as_ref(), kind.as_deref(), all, json),
        Command::Connect {
            connector,
            account,
            api_key_stdin,
            open,
        } => gateway_cmd::connect(target, &connector, account.as_deref(), api_key_stdin, open),
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
        | Command::Config { .. }
        | Command::Usage { .. }
        | Command::Remote { .. }
        | Command::Catalog { .. }
        | Command::Sync(_)
        | Command::Connectors { .. } => unreachable!("handled before choosing a target"),
    }
}

/// The `[fleet]` of the configuration under the current directory, for
/// commands that take no task options.
fn fleet_table() -> Result<Option<branchyard::Fleet>, Failure> {
    let cwd = std::env::current_dir()?;
    defaults::fleet_at(&cwd, &|name| std::env::var(name).ok()).map_err(Failure::Message)
}
