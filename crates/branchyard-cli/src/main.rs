//! `by`: delegate coding work to agent harnesses on git branches, and merge
//! only validated results. A thin client of the `branchyard` SDK in local
//! mode, and of a Branchyard server through `branchyard-client` in remote
//! mode (`--remote URL`).

// Doc comments on the clap types are the `--help` text, written for a shell
// reader (`[fleet]`, `HOST:PORT`, `<commit>`), not as rustdoc links or HTML.
#![allow(rustdoc::broken_intra_doc_links, rustdoc::invalid_html_tags)]

// `eprintln!` and `eprint!` for the whole crate, defined before the modules
// so that they are the ones the modules see. The standard ones panic when
// standard error cannot be written (`by run 2>&1 | head -1`, a full disk),
// which ended the command and, with it, a turn that had completed. What
// `by` says on standard error is for whoever is reading; once nobody is,
// there is nobody to tell, so a write that fails is dropped. (ripgrep does
// the same.) Output a command exists to produce goes through
// [`commands::print`], which returns its error.
macro_rules! eprintln {
    () => {
        $crate::write_stderr(format_args!("\n"))
    };
    ($($arg:tt)*) => {
        $crate::write_stderr(format_args!("{}\n", format_args!($($arg)*)))
    };
}

macro_rules! eprint {
    ($($arg:tt)*) => {
        $crate::write_stderr(format_args!($($arg)*))
    };
}

/// Write to standard error, ignoring a failure: see the `eprintln!` above.
fn write_stderr(args: std::fmt::Arguments<'_>) {
    use std::io::Write as _;
    match io::stderr().write_fmt(args) {
        Ok(()) | Err(_) => {}
    }
}

mod adf;
mod adopt;
mod args;
mod attempts;
mod catalog_cmd;
mod commands;
mod config_cmd;
mod console;
mod defaults;
mod detach;
mod doctor_cmd;
mod effects_cmd;
mod env_cmd;
mod errors;
mod fleet_cmd;
mod gateway_cmd;
mod gh;
mod harness_cmd;
mod hook_cmd;
mod inherited;
mod init;
mod json;
mod knowledge_cmd;
mod live_catalog;
mod map_cmd;
mod models_cmd;
mod models_list;
mod notify;
mod open;
mod operations;
mod plan_cmd;
mod ports;
mod pr;
mod pr_threads;
mod preflight;
mod recipe_cmd;
mod remote;
mod render;
mod review;
mod review_format;
mod rig;
mod services_cmd;
mod setup_cmd;
mod setup_io;
mod ssh_remote;
mod stats_cmd;
mod sync_cmd;
mod task_cmd;
mod trackers;
mod trigger_cmd;
mod up_cmd;
mod usage;
mod user_config;
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
    // A background command started by `--detach` takes its report pipe
    // before it starts anything that could inherit it.
    detach::become_child();
    args::unset_blank_env();
    if let Some(call) = args::server_call(&argv[1..]) {
        return serve(&argv[..=call.prefix], call);
    }
    // `by help all` and `by help agents` list commands; they are not commands.
    if let Some(listing) = args::help_listing(&argv[1..]) {
        return match commands::print(&args::listing_text(listing, Env::detect().color)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::FAILURE,
        };
    }
    let cli = match args::parse_from(&argv) {
        Ok(cli) => cli,
        // Help and --version come this way too, to stdout with exit 0;
        // usage errors go to stderr with exit 2.
        Err(error) => {
            // A refused command line is JSON too, when the line asks for it.
            if error.use_stderr() && errors::wants_json(&argv) {
                errors::print(&errors::usage(&error), true);
            } else {
                print_or_warn("print the help or usage message", error.print());
            }
            return ExitCode::from(error.exit_code() as u8);
        }
    };
    // No command: where this repository stands and the one next step, not
    // the sixty commands (`by --help` lists them).
    let Some(command) = cli.command else {
        return errors::finish(setup_cmd::card(), errors::wants_json(&argv));
    };
    // Run by a Claude Code session on every prompt and redraw: the
    // configuration only to find the yard, no logging, and never a failure, whatever the
    // repository holds (docs/harness-integration.md).
    match &command {
        Command::Hook { harness, event } => return hook_cmd::hook(harness.as_deref(), event),
        Command::Statusline { harness } => return hook_cmd::statusline(harness.as_deref()),
        _ => {}
    }
    // branchyard.toml and the user configuration, under flags and variables.
    let cwd = std::env::current_dir().unwrap_or_default();
    let env = |name: &str| std::env::var(name).ok();
    let (globals, command) = match defaults::apply(&cwd, &env, cli.globals, command) {
        Ok(applied) => applied,
        Err(error) => {
            let failure = errors::config(&error.to_string()).into();
            return errors::finish(Err(failure), errors::wants_json(&argv));
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
    let outcome = run(&env_now, &globals, command);
    detach::finish(&outcome);
    // The one place a failure is printed, and its kind's exit status
    // (docs/cli.md, "Exit status").
    errors::finish(outcome, errors::wants_json(&argv))
}

/// Print what `by` was asked to print (help, a usage error) and, when that
/// fails, say so on standard error: `by --help > /dev/full` once failed
/// without a word, because the subscriber that shows a survived failure was
/// not installed yet on this path. A closed pipe (`by --help | head -1`) is
/// the reader's choice and is not a failure.
fn print_or_warn(context: &str, result: io::Result<()>) {
    branchyard_server::logging::init_for_commands();
    match result {
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {}
        other => {
            branchyard_support::best_effort(context, other);
        }
    }
}

/// `by serve`, `by worker` and their help: the server's own command line.
/// `prefix` is `by`'s part, parsed only for its global options.
fn serve(prefix: &[OsString], call: args::ServerCall) -> ExitCode {
    let globals = match args::parse_from(prefix) {
        Ok(cli) => cli.globals,
        Err(error) => {
            print_or_warn("print the help or usage message", error.print());
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
        let refused = errors::Report::usage(format!(
            "{} runs a server here; it does not take --remote or BRANCHYARD_REMOTE",
            call.program
        ))
        .next(format!(
            "run {} on the server's host, without --remote (unset BRANCHYARD_REMOTE)",
            call.program
        ));
        return errors::finish(Err(refused.into()), false);
    }
    // `[serve] config` from branchyard.toml, unless the arguments name one.
    let cwd = std::env::current_dir().unwrap_or_default();
    let env = |name: &str| std::env::var(name).ok();
    let args = match defaults::apply_serve(&cwd, &env, call.args) {
        Ok(args) => args,
        Err(error) => return errors::finish(Err(errors::config(&error).into()), false),
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
        Command::Mcp {
            list_tools: true, ..
        } => return commands::mcp_tools(),
        Command::Mcp {
            root: Some(root),
            branch: Some(branch),
            ..
        } => return commands::mcp(&root, &branch),
        Command::Completions { shell } => {
            let mut out = Vec::new();
            clap_complete::generate(shell, &mut args::command(), "by", &mut out);
            return commands::print(&String::from_utf8_lossy(&out));
        }
        Command::Man { markdown: true } => return commands::print(&args::reference_markdown()),
        Command::Man { markdown: false } => {
            let mut out = Vec::new();
            clap_mangen::Man::new(args::command_with_hidden()).render(&mut out)?;
            return commands::print(&String::from_utf8_lossy(&out));
        }
        // Setup needs no repository or server: it may be what creates them.
        Command::Init { init } => return init::main(env, &init),
        Command::Setup { setup } => {
            // `--refresh`: the harnesses are asked again, not the cache.
            if setup.refresh {
                harness_cmd::forget_inventory();
            }
            return setup_cmd::main(env, &setup);
        }
        Command::Config { json, action } => return config_cmd::main(&action, json),
        // Doctor reports a broken configuration, a missing store or an
        // unreachable server rather than stopping at it.
        Command::Doctor {
            json,
            path,
            refresh,
        } => {
            if refresh {
                harness_cmd::forget_inventory();
            }
            return doctor_cmd::main(env, globals, path.as_deref(), json);
        }
        // Up acts on this machine's repository, whatever [remote] says.
        Command::Up { json, plan, yes } => return up_cmd::main(env, globals, plan, yes, json),
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
        return Err(errors::Report::usage(format!(
            "--provider recipe:{recipe} runs on the machine that has the repository: a server \
             does not run environment recipes (docs/recipes.md)"
        ))
        .next("run it without --remote, in the repository")
        .into());
    }
    let target = match &globals.remote {
        Some(_) => Target::Remote(Box::new(remote::Remote::connect(globals)?)),
        None if globals.token_file.is_some() || globals.repo.is_some() => {
            return Err(errors::Report::usage(
                "--token-file and --repo apply to remote mode; pass --remote URL too",
            )
            .next("add --remote URL (or set BRANCHYARD_REMOTE), or drop --token-file and --repo")
            .into())
        }
        None => Target::Local,
    };
    dispatch(env, &target, command)
}

/// One arm per command that runs against a repository or a server; see
/// "Adding a command" in `args/mod.rs`.
fn dispatch(env: &Env, target: &Target, command: Command) -> commands::Outcome {
    // `--auto` with no [fleet] routes among what is logged in here
    // (docs/fleet.md#the-inventory-default); a server refuses routing.
    let local = matches!(target, Target::Local);
    let inventory_default = |task: &mut args::TaskArgs| match local {
        true => fleet_cmd::with_inventory_default(task),
        false => Ok(()),
    };
    // `--detach`: the same command line again, in the background; that
    // command, which `detach` recognises, runs the turn as usual.
    if let Some(launch) = detach::requested(&command) {
        if !detach::become_child() {
            return detach::launch(target, &launch);
        }
    }
    match command {
        Command::Run {
            prompt,
            detached,
            mut task,
        } => {
            inventory_default(&mut task)?;
            // The background command of a `--detach` writes its log as text.
            let json = detached.json && !detached.detach;
            commands::run(env, target, &prompt, &task, json)
        }
        Command::Fan {
            prompt,
            harnesses,
            attempts,
            judge,
            json,
            mut task,
        } => {
            if harnesses.is_none() {
                inventory_default(&mut task)?;
            }
            commands::fan(
                env,
                target,
                &prompt,
                harnesses.as_ref().map(|h| h.0.as_slice()),
                &task,
                &commands::FanRoute {
                    attempts,
                    judge,
                    json,
                },
            )
        }
        Command::Map {
            json,
            action,
            mut map,
        } => {
            if action.is_none() {
                inventory_default(&mut map.task)?;
            }
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
            args::FleetAction::Advise { prompt, kind, seed } => fleet_cmd::advise(
                env,
                target,
                &prompt,
                kind,
                seed,
                fleet_table()?.as_ref(),
                json,
            ),
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
            prompt_file,
            task,
            steer: true,
            wait: _,
            json,
            ..
        } => {
            let prompt = match prompt_file {
                Some(path) => commands::read_prompt_file(&path)?,
                None => prompt,
            };
            commands::steer(target, &branch, &prompt, &task, "by send --steer", json)
        }
        Command::Send {
            branch,
            prompt,
            prompt_file,
            task,
            steer: false,
            retry,
            wait,
            json,
            detach,
        } => {
            // The background command of a `--detach` writes its log as text.
            let json = json && !detach;
            let prompt = match prompt_file {
                Some(path) => commands::read_prompt_file(&path)?,
                None => prompt,
            };
            let prompt = match retry {
                true => commands::Prompt::Retry,
                false => commands::Prompt::Text(&prompt),
            };
            commands::send(env, target, &branch, prompt, &task, wait, json)
        }
        Command::Steer {
            branch,
            prompt,
            prompt_file,
            json,
        } => {
            let prompt = match prompt_file {
                Some(path) => commands::read_prompt_file(&path)?,
                None => prompt,
            };
            commands::steer(
                target,
                &branch,
                &prompt,
                &args::TaskArgs::default(),
                "by steer",
                json,
            )
        }
        Command::Fork {
            branch,
            prompt,
            fresh_session,
            at: None,
            detached,
            task,
        } => {
            let json = detached.json && !detached.detach;
            commands::fork(env, target, &branch, &prompt, fresh_session, &task, json)
        }
        Command::Fork {
            branch,
            prompt,
            at: Some(turn),
            detached,
            task,
            ..
        } => {
            let json = detached.json && !detached.detach;
            commands::fork_at(env, target, &branch, turn, &prompt, &task, json)
        }
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
            json,
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
                json,
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
            json,
        } => {
            if promote_effects {
                effects_cmd::promote_before_merge(target, &branch)?;
            }
            commands::merge_as(target, &branch, into.as_deref(), rm, json)
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
            json,
        } => commands::rm(target, &branch, keep_credentials, json),
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
        Command::Discard {
            branch,
            reason,
            json,
        } => commands::discard(target, &branch, reason.as_deref(), json),
        Command::Spawn {
            prompt,
            prompt_file,
            spawn,
        } => {
            let prompt = match prompt_file {
                Some(path) => commands::read_prompt_file(&path)?,
                None => prompt,
            };
            commands::spawn(env, target, &prompt, &spawn)
        }
        Command::Delegate {
            plan,
            brief,
            land,
            parent,
            json,
            perms,
        } => {
            let mut task = args::TaskArgs::default();
            perms.apply(&mut task);
            commands::delegate(env, target, &plan, (brief, land), parent, &task, json)
        }
        Command::Brief {
            text,
            file,
            parent,
            json,
        } => {
            let text = match file {
                Some(path) => Some(commands::read_prompt_file(&path)?),
                None => text,
            };
            commands::brief(env, target, text, parent, json)
        }
        Command::Inspect { branch, json } => commands::inspect(env, target, branch, json),
        Command::Events {
            branch,
            cursor,
            limit,
            json,
        } => commands::events(env, target, branch, cursor, limit, json),
        Command::Integrate { branches, json } => commands::integrate(target, &branches, json),
        Command::Check { branch, json } => commands::check(env, target, branch, json),
        Command::Wait {
            branches,
            any,
            all: _,
            timeout,
            json,
        } => commands::wait(env, target, &branches, any, timeout, json),
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
            result,
            result_file,
            json,
        } => commands::report(
            target,
            as_branch,
            &text,
            (result.as_deref(), result_file.as_deref()),
            json,
        ),
        Command::Escalate {
            as_branch,
            text,
            json,
        } => commands::escalate(target, as_branch, &text, json),
        Command::ReportStatus {
            as_branch,
            state,
            summary,
            link,
            ttl,
            json,
        } => commands::report_status(
            target,
            as_branch,
            (&state, &summary, link.as_deref()),
            ttl,
            json,
        ),
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
        Command::Models {
            period,
            json,
            action,
        } => match action {
            Some(action) => models_list::main(env, target, &action, json),
            None => models_cmd::main(env, target, &period, json),
        },
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
        | Command::Man { .. }
        | Command::Init { .. }
        | Command::Setup { .. }
        | Command::Config { .. }
        | Command::Doctor { .. }
        | Command::Up { .. }
        | Command::Usage { .. }
        | Command::Remote { .. }
        | Command::Catalog { .. }
        | Command::Sync(_)
        | Command::Connectors { .. }
        | Command::Hook { .. }
        | Command::Statusline { .. } => unreachable!("handled before choosing a target"),
    }
}

/// The `[fleet]` of the configuration under the current directory, for
/// commands that take no task options.
fn fleet_table() -> Result<Option<branchyard::Fleet>, Failure> {
    let cwd = std::env::current_dir()?;
    defaults::fleet_at(&cwd, &|name| std::env::var(name).ok())
        .map_err(|why| errors::config(&why).into())
}
