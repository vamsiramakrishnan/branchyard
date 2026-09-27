//! `by init`: the setup interview's two front-ends. A person gets the
//! terminal wizard ([`crate::wizard`]); a harness or script gets the JSON
//! protocol (`--json --next`, `--dry-run`, `--apply`). Both run the same
//! [`branchyard_setup`] engine over this machine's probe and validators.
//! See `docs/setup.md`.

use std::collections::BTreeMap;
use std::io::Read;

use branchyard_setup::protocol::{ErrorBody, ErrorResponse, TopicList};
use branchyard_setup::{Response, Topic};
use serde_json::Value;

use crate::commands::{print, Env, Failure, Outcome};
use crate::setup_io::{self, HostProbe, HostValidator, RandomEntropy, Refusal};

pub const USAGE: &str = "\
Set up Branchyard by interview, in a terminal wizard or through a JSON
protocol a coding harness drives.

Usage: by init [TOPIC]                                   (wizard, on a terminal)
       by init --json                                    (the topics)
       by init TOPIC --json --next [--answers FILE|-]    (the next questions, or the plan)
       by init TOPIC --answers FILE|- --dry-run [--json] (the plan: files, diffs, validation)
       by init TOPIC --answers FILE|- --apply [--force] [--json]

Topics:
  project   branchyard.toml: default harness, model, limits, permissions,
            isolation, check, secrets by name, a server to use
  server    A server configuration: TLS, SQLite or PostgreSQL, tenants with
            hashed credentials and 0600 token files, quotas, providers
  rig       A rig: a lead seat and the seats it delegates to
  deploy    compose.yaml with PostgreSQL, a server configuration, secret files
  plugin    The setup and delegate skills, into Claude Code or Codex

Options:
  --json            Print JSON (schema/setup.protocol.json)
  --next            Print the next batch of at most four questions given the
                    answers so far, or the plan when none remain
  --answers FILE    A JSON object of answers by question id; - reads stdin
  --defaults        Take the default for every unanswered question
  --dry-run         Print the plan without writing anything
  --apply           Write the plan; refuses to replace a file that differs
  --force           With --apply: replace files that differ (shown as diffs)
  -h, --help        Show this help

Answers name where a secret is (a variable, or @file), never its value.
Generated tokens are written 0600 and printed nowhere. See docs/setup.md.
";

#[derive(Debug, Default, PartialEq)]
struct Flags {
    topic: Option<String>,
    json: bool,
    next: bool,
    answers: Option<String>,
    defaults: bool,
    dry_run: bool,
    apply: bool,
    force: bool,
    help: bool,
}

fn parse(args: &[String]) -> Result<Flags, String> {
    let mut flags = Flags::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value.to_owned())),
            _ => (arg.as_str(), None),
        };
        let switch = |flag: &mut bool| match inline {
            None if !*flag => {
                *flag = true;
                Ok(())
            }
            None => Err(format!("{name} given twice")),
            Some(_) => Err(format!("{name} takes no value")),
        };
        match name {
            "-h" | "--help" => flags.help = true,
            "--json" => switch(&mut flags.json)?,
            "--next" => switch(&mut flags.next)?,
            "--defaults" => switch(&mut flags.defaults)?,
            "--dry-run" => switch(&mut flags.dry_run)?,
            "--apply" => switch(&mut flags.apply)?,
            "--force" => switch(&mut flags.force)?,
            "--answers" => {
                if flags.answers.is_some() {
                    return Err("--answers given twice".into());
                }
                let value = match inline {
                    Some(value) => value,
                    None => args
                        .next()
                        .cloned()
                        .ok_or("--answers needs a value FILE or -")?,
                };
                flags.answers = Some(value);
            }
            other if other.starts_with('-') => return Err(format!("unknown option {other}")),
            topic if flags.topic.is_none() => flags.topic = Some(topic.to_owned()),
            extra => return Err(format!("unexpected argument '{extra}'")),
        }
    }
    let modes = [flags.next, flags.dry_run, flags.apply]
        .iter()
        .filter(|m| **m)
        .count();
    if modes > 1 {
        return Err("--next, --dry-run and --apply are separate steps; give one".into());
    }
    if flags.force && !flags.apply {
        return Err("--force applies to --apply".into());
    }
    if modes == 1 && flags.topic.is_none() {
        return Err("give a topic: project, server, rig, deploy or plugin".into());
    }
    Ok(flags)
}

fn usage_error(message: &str) -> Outcome {
    eprintln!("by: {message}\nTry 'by init --help'.");
    std::process::exit(2);
}

/// Print a refusal: JSON on stdout with `--json`, else a message.
fn refuse(json: bool, kind: &str, message: String, paths: Vec<String>) -> Outcome {
    if json {
        let body = ErrorResponse {
            error: ErrorBody {
                kind: kind.into(),
                message,
                paths,
            },
        };
        print(&format!(
            "{}\n",
            serde_json::to_string_pretty(&body).unwrap_or_default()
        ))?;
        return Err(Failure::Reported);
    }
    let mut text = message;
    for path in paths {
        text.push_str(&format!("\n  {path}"));
    }
    Err(Failure::Message(text))
}

fn read_answers(source: &str) -> Result<BTreeMap<String, Value>, String> {
    let text = match source {
        "-" => {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .map_err(|e| format!("stdin: {e}"))?;
            text
        }
        path => std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?,
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Ok(map.into_iter().collect()),
        Ok(_) => Err(format!(
            "{source}: answers are a JSON object keyed by question id"
        )),
        Err(e) => Err(format!("{source}: {e}")),
    }
}

pub fn main(env: &Env, args: &[String]) -> Outcome {
    let flags = match parse(args) {
        Ok(flags) => flags,
        Err(message) => return usage_error(&message),
    };
    if flags.help {
        return print(USAGE);
    }
    let topic = match flags.topic.as_deref().map(|t| (t, Topic::parse(t))) {
        None => None,
        Some((_, Some(topic))) => Some(topic),
        Some((other, None)) => {
            return usage_error(&format!(
                "unknown topic '{other}'; use project, server, rig, deploy or plugin"
            ))
        }
    };
    let machine = flags.next || flags.dry_run || flags.apply;
    if !machine {
        if flags.json {
            if topic.is_some() {
                return usage_error("with --json, give --next, --dry-run or --apply");
            }
            return print(&format!(
                "{}\n",
                serde_json::to_string_pretty(&TopicList::new()).unwrap_or_default()
            ));
        }
        if flags.answers.is_some() {
            return usage_error("--answers needs --next, --dry-run or --apply");
        }
        if !(env.stdin_tty && env.stdout_tty) {
            eprintln!(
                "by: by init asks questions on a terminal, and none is attached.\n\
                 Harnesses and scripts use the JSON protocol instead:\n\
                 \x20 by init --json                              # the topics\n\
                 \x20 by init project --json --next               # the first questions\n\
                 \x20 by init project --answers FILE --dry-run    # the plan\n\
                 See docs/setup.md, or the setup skill (by init plugin)."
            );
            std::process::exit(2);
        }
        return crate::wizard::run(topic, flags.defaults);
    }
    let topic = topic.expect("checked in parse");
    let raw = match &flags.answers {
        Some(source) => match read_answers(source) {
            Ok(raw) => raw,
            Err(message) => return refuse(flags.json, "invalid_answers", message, Vec::new()),
        },
        None => BTreeMap::new(),
    };
    let cwd = std::env::current_dir()?;
    let probe = HostProbe::new(&cwd);
    let response = branchyard_setup::next(
        topic,
        &probe,
        &raw,
        flags.defaults,
        &mut RandomEntropy,
        &HostValidator,
    );
    if flags.next {
        return print(&format!("{}\n", to_json(&response)));
    }
    step(&probe, &flags, response)
}

fn to_json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// `--dry-run` or `--apply` once the answers are in.
fn step(probe: &HostProbe, flags: &Flags, response: Response) -> Outcome {
    let Some(plan) = &response.plan else {
        let mut pending: Vec<String> = response
            .errors
            .iter()
            .map(|e| format!("{}: {}", e.id, e.message))
            .collect();
        pending.extend(
            response
                .questions
                .iter()
                .filter(|q| !response.errors.iter().any(|e| e.id == q.id))
                .map(|q| format!("{}: unanswered", q.id)),
        );
        let remaining = response.questions.len() + response.remaining;
        return refuse(
            flags.json,
            "incomplete",
            format!(
                "{remaining} question(s) remain; run by init {} --json --next with these answers, or pass --defaults",
                response.topic.id()
            ),
            pending,
        );
    };
    if flags.dry_run {
        return match flags.json {
            true => print(&format!("{}\n", to_json(&response))),
            false => print(&crate::wizard::review_text(plan, false)),
        };
    }
    match setup_io::apply(plan, probe.root_path(), flags.force) {
        Ok(applied) => match flags.json {
            true => print(&format!("{}\n", to_json(&applied))),
            false => {
                let mut text = String::new();
                for path in &applied.written {
                    text.push_str(&format!("wrote {path}\n"));
                }
                for path in &applied.unchanged {
                    text.push_str(&format!("unchanged {path}\n"));
                }
                if !applied.commands.is_empty() {
                    text.push_str("\nNext:\n");
                    for c in &applied.commands {
                        text.push_str(&format!("  {}\n      {}\n", c.command, c.why));
                    }
                }
                print(&text)
            }
        },
        Err(Refusal::Invalid(problems)) => refuse(
            flags.json,
            "invalid_plan",
            "the plan failed validation; nothing was written".into(),
            problems,
        ),
        Err(Refusal::WouldOverwrite(paths)) => refuse(
            flags.json,
            "would_overwrite",
            "these files exist with different content; review the diffs (--dry-run) and pass --force to replace them; nothing was written".into(),
            paths,
        ),
        Err(Refusal::Io(message)) => refuse(flags.json, "io", message, Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn flags_parse_and_conflict() {
        let flags = parse(&args("project --json --next --answers=a.json")).unwrap();
        assert_eq!(flags.topic.as_deref(), Some("project"));
        assert!(flags.json && flags.next);
        assert_eq!(flags.answers.as_deref(), Some("a.json"));
        for (line, error) in [
            ("project --next --apply", "separate steps"),
            ("project --force", "--force applies to --apply"),
            ("--next", "give a topic"),
            ("project extra", "unexpected argument"),
            ("project --bogus", "unknown option"),
            ("project --json --json", "given twice"),
            ("project --answers", "needs a value"),
        ] {
            let got = parse(&args(line)).unwrap_err();
            assert!(got.contains(error), "{line}: {got}");
        }
    }
}
