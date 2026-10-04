//! `by init`: the setup interview's two front-ends. A person gets the
//! terminal wizard ([`crate::wizard`]); a harness or script gets the JSON
//! protocol (`--json --next`, `--dry-run`, `--apply`). Both run the same
//! [`branchyard_setup`] engine over this machine's probe and validators.
//! See `docs/setup.md`.

use std::collections::BTreeMap;
use std::io::Read;

use branchyard_setup::protocol::{ErrorBody, ErrorResponse, TopicList};
use branchyard_setup::Response;
use serde_json::Value;

use crate::args::{InitArgs, InitStep};
use crate::commands::{print, Env, Failure, Outcome};
use crate::setup_io::{self, HostProbe, HostValidator, RandomEntropy, Refusal};

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

/// `by init`, its flags already checked by clap (`crate::args::InitFlags`).
#[allow(clippy::expect_used)] // ratchet: branchyard-cli
pub fn main(env: &Env, init: &InitArgs) -> Outcome {
    let Some(step) = init.step else {
        if init.json {
            // A topic with --json and no step is refused while parsing.
            return print(&format!(
                "{}\n",
                serde_json::to_string_pretty(&TopicList::new()).unwrap_or_default()
            ));
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
        return crate::wizard::run(init.topic, init.defaults);
    };
    let topic = init.topic.expect("clap requires a topic with a step");
    let raw = match &init.answers {
        Some(source) => match read_answers(source) {
            Ok(raw) => raw,
            Err(message) => return refuse(init.json, "invalid_answers", message, Vec::new()),
        },
        None => BTreeMap::new(),
    };
    let cwd = std::env::current_dir()?;
    let probe = HostProbe::new(&cwd);
    let response = branchyard_setup::next(
        topic,
        &probe,
        &raw,
        init.defaults,
        &mut RandomEntropy,
        &HostValidator,
    );
    if step == InitStep::Next {
        return print(&format!("{}\n", to_json(&response)));
    }
    finish(&probe, init, step, response)
}

fn to_json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// `--dry-run` or `--apply` once the answers are in.
fn finish(probe: &HostProbe, flags: &InitArgs, step: InitStep, response: Response) -> Outcome {
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
    if step == InitStep::DryRun {
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
