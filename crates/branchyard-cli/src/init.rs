//! `by init`: the setup interview's two front-ends. A person gets the
//! terminal wizard ([`crate::wizard`]); a harness or script gets the JSON
//! protocol (`--json --next`, `--dry-run`, `--apply`). Both run the same
//! [`branchyard_setup`] engine over this machine's probe and validators.
//! See `docs/setup.md`.

use std::collections::BTreeMap;
use std::io::Read;

use branchyard_setup::install::{self, HarnessList};
use branchyard_setup::protocol::{ErrorBody, ErrorResponse, TopicList};
use branchyard_setup::{Facts, Probe, Response, Topic};
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

/// `by init plugin` with harnesses named, `--all-detected`, or nothing:
/// the listing, or the install plan for those harnesses. `None` when the
/// ordinary interview takes over (a protocol step with no harness named,
/// or the wizard on a terminal after the listing).
fn plugin(env: &Env, init: &InitArgs) -> Option<Outcome> {
    let named = !init.harnesses.is_empty() || init.all_detected;
    if !named && init.step.is_some() {
        return None;
    }
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => return Some(Err(e.into())),
    };
    // Installing hooks needs no harness versions, and running some
    // harnesses writes their directories into the home.
    let probe = HostProbe::without_versions(&cwd);
    if !named {
        // The listing: no harness versions are asked for, only PATH.
        let facts = Facts {
            home: probe.home(),
            install_detected: install::detect(&probe),
            ..Facts::default()
        };
        let list = match install::listing(&facts) {
            Ok(list) => list,
            Err(e) => return Some(Err(Failure::Message(e))),
        };
        if init.json {
            return Some(print(&format!("{}\n", to_json(&list))));
        }
        if let Err(e) = print(&listing_text(&list)) {
            return Some(Err(e));
        }
        return match env.stdin_tty && env.stdout_tty {
            // Then the interview, which offers the skills too.
            true => None,
            false => Some(Ok(())),
        };
    }
    let known: Vec<String> = match install::manifests() {
        Ok(all) => all.into_iter().map(|m| m.harness).collect(),
        Err(e) => return Some(Err(Failure::Message(e))),
    };
    let ids: Vec<String> = match init.all_detected {
        true => install::detect(&probe),
        false => init.harnesses.clone(),
    };
    if let Some(unknown) = ids.iter().find(|id| !known.contains(id)) {
        return Some(Err(Failure::Message(format!(
            "no install for harness {unknown:?}; by init plugin installs for: {}",
            known.join(", ")
        ))));
    }
    if ids.is_empty() {
        return Some(print(
            "No harness Branchyard installs for is on PATH; nothing to do.\nby init plugin lists them all; name one to install for it anyway.\n",
        ));
    }
    let mut raw = match &init.answers {
        Some(source) => match read_answers(source) {
            Ok(raw) => raw,
            Err(message) => return Some(refuse(init.json, "invalid_answers", message, Vec::new())),
        },
        None => BTreeMap::new(),
    };
    raw.insert("host".into(), Value::String("harnesses".into()));
    raw.insert(
        "harnesses".into(),
        Value::Array(ids.into_iter().map(Value::String).collect()),
    );
    let response = branchyard_setup::next(
        Topic::Plugin,
        &probe,
        &raw,
        true,
        &mut RandomEntropy,
        &HostValidator,
    );
    Some(match init.step {
        Some(InitStep::Next) => print(&format!("{}\n", to_json(&response))),
        Some(step) => finish(&probe, init, step, response),
        None if init.json => finish(&probe, init, InitStep::DryRun, response),
        None => review_then_ask(env, &probe, response),
    })
}

/// `by init plugin`'s listing as text.
fn listing_text(list: &HarnessList) -> String {
    let mut out = String::from(
        "Harnesses by init plugin installs for (detected: one of its programs is on PATH):\n",
    );
    for h in &list.harnesses {
        let found = match h.detected {
            true => "detected",
            false => "not found",
        };
        out.push_str(&format!(
            "\n{} ({})  {found} ({})  gets {}\n",
            h.label,
            h.harness,
            h.binaries.join(", "),
            h.provides.join(", ")
        ));
        for f in &h.files {
            let at = match f.at.as_str() {
                "" => String::new(),
                at => format!(" at {at}"),
            };
            out.push_str(&format!("    {} {}{at}\n", f.how.label(), f.path));
        }
    }
    out.push_str(
        "\nInstall: by init plugin ID... (or --all-detected). It shows every file it would write or merge, then asks; --dry-run only shows, --apply writes without asking. See docs/harness-hooks.md.\n",
    );
    out
}

/// Show the plan's diffs and ask, on a terminal; elsewhere, say how to
/// write it.
fn review_then_ask(env: &Env, probe: &HostProbe, response: Response) -> Outcome {
    let Some(plan) = &response.plan else {
        return Err(Failure::Message(
            "the install finished without a plan".into(),
        ));
    };
    print(&crate::wizard::review_text(plan, env.color))?;
    if !plan.valid {
        return Err(Failure::Message(
            "a file cannot be installed (above); nothing was written".into(),
        ));
    }
    let writes = plan.files.iter().filter(|f| f.writes()).count();
    if writes == 0 {
        return print("\nEverything is already installed; nothing to write.\n");
    }
    if !(env.stdin_tty && env.stdout_tty) {
        return print(
            "\nNothing was written. Run again with --apply to write these files (and --force to replace the ones marked update that are not merges).\n",
        );
    }
    let overwrites = plan.overwrites();
    let mut force = false;
    if !overwrites.is_empty() {
        let names: Vec<&str> = overwrites.iter().map(|f| f.path.as_str()).collect();
        force = cliclack::confirm(format!(
            "Replace {} (see the diff above)?",
            names.join(", ")
        ))
        .initial_value(false)
        .interact()?;
        if !force {
            return print("Nothing was written.\n");
        }
    }
    let go = cliclack::confirm(format!("Write {writes} file(s)?"))
        .initial_value(true)
        .interact()?;
    if !go {
        return print("Nothing was written.\n");
    }
    let flags = InitArgs {
        topic: Some(Topic::Plugin),
        step: Some(InitStep::Apply),
        json: false,
        answers: None,
        defaults: true,
        force,
        harnesses: Vec::new(),
        all_detected: false,
    };
    finish(probe, &flags, InitStep::Apply, response)
}

/// `by init`, its flags already checked by clap (`crate::args::InitFlags`).
pub fn main(env: &Env, init: &InitArgs) -> Outcome {
    if init.topic == Some(Topic::Plugin) {
        if let Some(outcome) = plugin(env, init) {
            return outcome;
        }
    }
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
    let Some(topic) = init.topic else {
        return Err(Failure::Message("a step needs a topic".into()));
    };
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
                // A harness install's notes: what only the person can do.
                if plan.files.iter().any(|f| f.how.is_some()) && !plan.notes.is_empty() {
                    text.push('\n');
                    for note in &plan.notes {
                        text.push_str(&format!("note: {note}\n"));
                    }
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
