//! `branchyard-herdr`: a Herdr plugin that presents a Branchyard server's
//! branches in Herdr. See `plugins/herdr/README.md`.
//!
//! Herdr runs each of these from the plugin's `herdr-plugin.toml`:
//!
//! - `bridge`: follow the server's activity feed; one tab per branch
//!   running `by log --follow`, with the branch's state reported on it.
//! - `log`: the command in a branch tab (`BRANCHYARD_HERDR_BRANCH`).
//! - `start`: open the bridge in a tab.
//! - `action merge|cancel|send`: act on the branch in the focused pane.
//! - `send`: the popup that reads a prompt and runs `by send`.

mod actions;
mod bridge;
mod config;
mod herdr;
mod model;

use std::io::IsTerminal;
use std::process::ExitCode;

use config::Config;

const USAGE: &str = "\
usage: branchyard-herdr [--remote URL] [--token-file FILE] [--repo NAME] [--ca-file FILE] COMMAND

commands:
  bridge                 follow the server and keep one Herdr pane per branch
  start                  open the bridge in a new Herdr tab
  log                    run `by log --follow $BRANCHYARD_HERDR_BRANCH` (a branch pane)
  action merge|cancel|send
                         act on the branch shown in the focused pane
  send                   read a prompt and run `by send $BRANCHYARD_HERDR_BRANCH` (a popup)

Settings not given as flags come from BRANCHYARD_REMOTE, BRANCHYARD_TOKEN_FILE,
BRANCHYARD_REPO and BRANCHYARD_CA_FILE, then from config.env in the plugin's
Herdr config directory.
";

fn main() -> ExitCode {
    let mut flags = Vec::new();
    let mut rest = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let name = match arg.as_str() {
            "--remote" => "BRANCHYARD_REMOTE",
            "--token-file" => "BRANCHYARD_TOKEN_FILE",
            "--repo" => "BRANCHYARD_REPO",
            "--ca-file" => "BRANCHYARD_CA_FILE",
            "-h" | "--help" | "help" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            _ => {
                rest.push(arg);
                continue;
            }
        };
        match args.next() {
            Some(value) => flags.push((name.to_owned(), value)),
            None => return usage(&format!("{arg} needs a value")),
        }
    }
    let config = Config::load(&flags);
    let result = match rest.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["bridge"] => bridge::run(&config),
        ["start"] => actions::start(&config),
        ["log"] => actions::log(&config),
        ["send"] => actions::send_popup(&config),
        ["action", name] => actions::action(&config, name),
        [] => return usage("a command is required"),
        _ => return usage(&format!("unknown command: {}", rest.join(" "))),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("branchyard-herdr: {message}");
            // In a Herdr pane, which closes when this exits: keep the
            // reason on screen.
            if rest == ["bridge"] && std::io::stdin().is_terminal() {
                eprint!("press Enter to close ");
                let _ = std::io::stdin().read_line(&mut String::new());
            }
            ExitCode::FAILURE
        }
    }
}

fn usage(message: &str) -> ExitCode {
    eprintln!("branchyard-herdr: {message}\n\n{USAGE}");
    ExitCode::from(2)
}
