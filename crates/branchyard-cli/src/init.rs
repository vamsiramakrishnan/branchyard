//! `by init`: the setup interview's two front-ends. A person gets the
//! terminal wizard ([`crate::wizard`]); a harness or script gets the JSON
//! protocol (`--json --next`, `--dry-run`, `--apply`). Both run the same
//! [`branchyard_setup`] engine over this machine's probe and validators.
//! See `docs/setup.md`.

use std::collections::BTreeMap;
use std::io::Read;

use branchyard_setup::install::{self, HarnessList};
use branchyard_setup::protocol::TopicList;
use branchyard_setup::{ArtifactKind, Facts, Plan, Probe, Response, Topic};
use serde_json::Value;

use crate::args::{InitArgs, InitStep};
use crate::commands::{print, Env, Outcome};
use crate::errors::Report;
use crate::setup_io::{self, HostProbe, HostValidator, RandomEntropy, Refusal};

/// A refusal of the setup protocol: `{"error": {"kind", "message",
/// "paths", "exit"}}` on stdout with `--json`, else the message with the
/// paths under it.
fn refuse(json: bool, kind: &str, message: String, paths: Vec<String>) -> Outcome {
    let mut report = Report::new(kind, message);
    match json {
        true if !paths.is_empty() => report = report.field("paths", serde_json::json!(paths)),
        true => {}
        false => {
            for path in paths {
                report = report.line("", path);
            }
        }
    }
    Err(report.with_json(json).into())
}

/// A usage error, as clap's are: the message, exit 2. With `--json`, the
/// refusal document on stdout.
fn usage(json: bool, message: String) -> Outcome {
    Err(Report::usage(message).with_json(json).into())
}

/// Whether `cwd` is inside a repository, as setup finds the root.
pub(crate) fn in_work_tree(cwd: &std::path::Path) -> bool {
    branchyard_config::is_repository(&setup_io::project_root(cwd))
}

/// What `by init project` and `by up` say outside a repository.
pub(crate) fn not_a_repository(cwd: &std::path::Path) -> String {
    format!(
        "not a git repository: {}. Run `git init` here, or cd into one",
        cwd.display()
    )
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
    if init.status {
        return Some(crate::setup_cmd::plugin_status(
            env,
            &init.harnesses,
            init.json,
        ));
    }
    let named = !init.harnesses.is_empty() || init.all_detected;
    match init.step {
        // The protocol asks which harnesses, or takes them from answers.
        Some(InitStep::Next) => return None,
        Some(_) if !named && (init.answers.is_some() || init.defaults) => return None,
        Some(_) if !named => {
            return Some(usage(
                init.json,
                "name harness IDs or --all-detected; `by init plugin` lists them".into(),
            ))
        }
        _ => {}
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
            Err(e) => return Some(Err(Report::new("io", e).into())),
        };
        if init.json {
            return Some(print(&format!("{}\n", to_json(&list))));
        }
        if let Err(e) = print(&listing_text(&list)) {
            return Some(Err(e));
        }
        return match env.stdin_tty && env.stdout_tty {
            // Then the interview: which of them, the detected ones chosen.
            true => None,
            false => Some(Ok(())),
        };
    }
    let known: Vec<String> = match install::manifests() {
        Ok(all) => all.into_iter().map(|m| m.harness).collect(),
        Err(e) => return Some(Err(Report::new("io", e).into())),
    };
    let ids: Vec<String> = match init.all_detected {
        true => install::detect(&probe),
        false => init.harnesses.clone(),
    };
    if let Some(unknown) = ids.iter().find(|id| !known.contains(id)) {
        return Some(Err(Report::usage(format!(
            "no install for harness {unknown:?}; by init plugin installs for: {}",
            known.join(", ")
        ))
        .next("by init plugin (lists them, and which are on PATH)")
        .into()));
    }
    if ids.is_empty() {
        return Some(print(
            "No harness Branchyard installs for is on PATH; nothing to do.\nby init plugin lists them all; name one to install for it anyway.\n",
        ));
    }
    let raw = match &init.answers {
        Some(source) => match read_answers(source) {
            Ok(raw) => raw,
            Err(message) => return Some(refuse(init.json, "invalid_answers", message, Vec::new())),
        },
        None => BTreeMap::new(),
    };
    let response = plugin_response(&probe, ids, raw);
    Some(match init.step {
        Some(InitStep::Next) => print(&format!("{}\n", to_json(&response))),
        Some(step) => finish(&probe, init, step, response),
        None if init.json => finish(&probe, init, InitStep::DryRun, response),
        None => review_then_ask(env, &probe, init, response),
    })
}

/// The plan that installs hooks, commands and skills for the harnesses
/// `ids` (the plugin topic with `host = harnesses`), over `answers`. `by
/// init plugin` and `by setup` both install through it.
pub(crate) fn plugin_response(
    probe: &dyn Probe,
    ids: Vec<String>,
    mut answers: BTreeMap<String, Value>,
) -> Response {
    answers.insert("host".into(), Value::String("harnesses".into()));
    answers.insert(
        "harnesses".into(),
        Value::Array(ids.into_iter().map(Value::String).collect()),
    );
    branchyard_setup::next(
        Topic::Plugin,
        probe,
        &answers,
        true,
        &mut RandomEntropy,
        &HostValidator,
    )
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
            let unless = match f.unless_set.as_str() {
                "" => String::new(),
                key => format!(", only if it sets no \"{key}\""),
            };
            out.push_str(&format!("    {} {}{at}{unless}\n", f.how.label(), f.path));
        }
    }
    out.push_str(
        "\nInstall: by init plugin ID... (or --all-detected). It shows every file it would write or merge, then asks; --dry-run only shows, --apply writes without asking. See docs/harness-hooks.md.\n",
    );
    out
}

/// Show the plan's diffs and ask, on a terminal; elsewhere, say how to
/// write it.
fn review_then_ask(env: &Env, probe: &HostProbe, init: &InitArgs, response: Response) -> Outcome {
    let Some(plan) = &response.plan else {
        return Err(Report::new("invalid_plan", "the install finished without a plan").into());
    };
    let terminal = env.stdin_tty && env.stdout_tty;
    // On a terminal it asks next, and says what follows once written;
    // elsewhere the next thing is writing it.
    let shown = match terminal {
        true => Plan {
            commands: Vec::new(),
            ..plan.clone()
        },
        false => before_writing(plan, init),
    };
    print(&crate::wizard::review_text(&shown, env.color))?;
    if !plan.valid {
        return Err(Report::new(
            "invalid_plan",
            "a file cannot be installed (above); nothing was written",
        )
        .next("fix what the plan names, then by init plugin again")
        .into());
    }
    let writes = plan.files.iter().filter(|f| f.writes()).count();
    if writes == 0 {
        return print("\nEverything is already installed; nothing to write.\n");
    }
    if !terminal {
        return Ok(());
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
        status: false,
    };
    finish(probe, &flags, InitStep::Apply, response)
}

/// `by init`, its flags already checked by clap (`crate::args::InitFlags`).
pub fn main(env: &Env, init: &InitArgs) -> Outcome {
    if init.topic == Some(Topic::Project) {
        let cwd = std::env::current_dir()?;
        if !in_work_tree(&cwd) {
            return refuse(
                init.json,
                "not_a_repository",
                not_a_repository(&cwd),
                Vec::new(),
            );
        }
    }
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
            return Err(Report::usage(
                "by init asks questions on a terminal, and none is attached.\n\
                 Harnesses and scripts use the JSON protocol instead:\n\
                 \x20 by init --json                              # the topics\n\
                 \x20 by init project --json --next               # the first questions\n\
                 \x20 by init project --answers FILE --dry-run    # the plan\n\
                 See docs/setup.md, or the setup skill (by init plugin).",
            )
            .into());
        }
        return crate::wizard::run(init.topic, init.defaults);
    };
    let Some(topic) = init.topic else {
        return Err(Report::usage("a step needs a topic")
            .next("by init project --json --next (or server, rig, deploy, plugin)")
            .into());
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

/// The command that writes what `init` asked for: `by init plugin codex
/// --apply`, or the same with the answers file or `--defaults` it was given
/// (not stdin, which cannot be read twice).
fn apply_command(init: &InitArgs) -> Option<String> {
    let mut command = String::from("by init plugin");
    match (init.all_detected, init.harnesses.is_empty()) {
        (true, _) => command.push_str(" --all-detected"),
        (false, false) => {
            for id in &init.harnesses {
                command.push(' ');
                command.push_str(id);
            }
        }
        (false, true) => match (&init.answers, init.defaults) {
            (Some(file), _) if file != "-" => command.push_str(&format!(" --answers {file}")),
            (None, true) => command.push_str(" --defaults"),
            _ => return None,
        },
    }
    command.push_str(" --apply");
    Some(command)
}

/// `plan` as shown when nothing is written yet: a harness install is
/// followed by the command that writes it, and nothing else (what comes
/// after is printed by `--apply`). A plan that cannot be written, or has
/// nothing to write, offers no command.
fn before_writing(plan: &Plan, init: &InitArgs) -> Plan {
    let mut shown = plan.clone();
    if plan.topic != Topic::Plugin {
        return shown;
    }
    shown.commands.clear();
    let writes = plan.files.iter().any(|f| f.writes());
    if let Some(command) = apply_command(init).filter(|_| plan.valid && writes) {
        let force = match plan.overwrites().is_empty() {
            true => "",
            false => " (--force also replaces the files marked update that are not merges)",
        };
        shown.command(
            command,
            format!("Write these files; nothing has been written yet{force}."),
        );
    }
    shown
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
            false => {
                let mut text = crate::wizard::review_text(&before_writing(plan, flags), false);
                if plan.topic == Topic::Plugin
                    && plan.valid
                    && !plan.files.iter().any(|f| f.writes())
                {
                    text.push_str("\nEverything is already installed; nothing to write.\n");
                }
                print(&text)
            }
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
        Err(Refusal::WouldOverwrite(paths)) => {
            // A configuration file `by` itself refuses is not "different
            // content": say it does not load, and that --force replaces it.
            let unloadable = paths.iter().find_map(|path| {
                let file = plan
                    .files
                    .iter()
                    .find(|f| f.path == *path && f.kind == ArtifactKind::ProjectConfig)?;
                // Unreadable here means the plain refusal below, which still
                // names the file.
                let text = branchyard_support::best_effort(
                    "read the configuration file by refuses to overwrite",
                    std::fs::read_to_string(probe.resolve(&file.path)),
                )?;
                let problem = branchyard_setup::hint::unreadable(&text)?;
                let line = problem.line.map(|n| format!(" (line {n})")).unwrap_or_default();
                Some(format!(
                    "{path} does not load{line}; --force replaces it. See what is wrong with `by config validate`; nothing was written"
                ))
            });
            match (unloadable, paths.len()) {
                // The message names the file; the text needs no list under it.
                (Some(message), 1) => refuse(
                    flags.json,
                    "would_overwrite",
                    message,
                    if flags.json { paths } else { Vec::new() },
                ),
                _ => refuse(
                    flags.json,
                    "would_overwrite",
                    "these files exist with different content; review the diffs (--dry-run) and pass --force to replace them; nothing was written".into(),
                    paths,
                ),
            }
        }
        Err(Refusal::Io(message)) => refuse(flags.json, "io", message, Vec::new()),
    }
}
