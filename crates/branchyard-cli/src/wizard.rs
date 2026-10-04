//! `by init`'s terminal wizard, over `cliclack`. The engine decides every
//! question, default and file; this only maps a [`Question`] onto a prompt
//! ([`widget`], pure and tested) and a plan onto a review ([`review_text`],
//! pure and tested), then asks, shows the diffs and writes only after the
//! person agrees.

use std::collections::BTreeMap;
use std::io;

use branchyard_setup::plan::{FileAction, Plan};
use branchyard_setup::{Facts, Kind, Question, Topic};
use serde_json::Value;

use crate::commands::{Failure, Outcome};
use crate::setup_io::{self, CachedProbe, HostProbe, HostValidator, RandomEntropy, Refusal};

/// How a question is asked on the terminal.
#[derive(Clone, Debug, PartialEq)]
pub enum Widget {
    /// Pick one; `other` adds a last item that opens a text input.
    Select {
        items: Vec<(Value, String, String)>,
        initial: usize,
        other: bool,
    },
    /// Pick any; `other` then asks for more names as text.
    MultiSelect {
        items: Vec<(Value, String, String)>,
        initial: Vec<usize>,
        other: bool,
    },
    Confirm {
        initial: bool,
    },
    /// Free text, with the default shown as a placeholder.
    Input {
        default: Option<String>,
    },
}

/// The label of the item that opens a text input.
pub const OTHER: &str = "Type a value";

fn display(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

pub fn widget(q: &Question) -> Widget {
    let items: Vec<(Value, String, String)> = q
        .choices
        .iter()
        .chain(&q.more_choices)
        .map(|c| (c.value.clone(), c.label.clone(), c.description.clone()))
        .collect();
    match q.kind {
        Kind::Confirm => Widget::Confirm {
            initial: q.default.as_bool().unwrap_or(false),
        },
        Kind::Multiselect => {
            let defaults = q.default.as_array().cloned().unwrap_or_default();
            Widget::MultiSelect {
                initial: items
                    .iter()
                    .enumerate()
                    .filter(|(_, (v, _, _))| defaults.contains(v))
                    .map(|(i, _)| i)
                    .collect(),
                items,
                other: q.allow_other,
            }
        }
        _ if items.is_empty() => Widget::Input {
            default: display(&q.default),
        },
        _ => Widget::Select {
            initial: items
                .iter()
                .position(|(v, _, _)| *v == q.default)
                .unwrap_or(0),
            items,
            other: q.allow_other,
        },
    }
}

const RESET: &str = "\x1b[0m";

fn paint(color: bool, code: &str, text: &str) -> String {
    match color {
        true => format!("\x1b[{code}m{text}{RESET}"),
        false => text.to_owned(),
    }
}

/// The review of a plan: summary, each file with its action, mode, diff
/// and validation, notes and next commands. Secrets show no content.
pub fn review_text(plan: &Plan, color: bool) -> String {
    let mut out = String::new();
    for line in &plan.summary {
        out.push_str(&format!("{line}\n"));
    }
    for file in &plan.files {
        let action = match file.action {
            FileAction::Create => paint(color, "32", "create"),
            FileAction::Update => paint(color, "33", "update"),
            FileAction::Unchanged => paint(color, "2", "unchanged"),
            FileAction::Keep => paint(color, "2", "keep"),
        };
        let verdict = match (&file.validation.ok, file.validation.skipped) {
            (true, true) => paint(
                color,
                "2",
                &format!("not checked: {}", file.validation.messages.join("; ")),
            ),
            (true, false) => paint(color, "32", &format!("✓ {}", file.validation.validator)),
            (false, _) => paint(
                color,
                "31",
                &format!(
                    "✗ {}: {}",
                    file.validation.validator,
                    file.validation.messages.join("; ")
                ),
            ),
        };
        out.push_str(&format!(
            "\n{action} {} (mode {})  {verdict}\n",
            file.path, file.mode
        ));
        if file.sensitive {
            out.push_str("  generated secret; its content is never shown\n");
        }
        if let Some(diff) = &file.diff {
            for line in diff.lines() {
                let painted = match line.chars().next() {
                    _ if line.starts_with("+++") || line.starts_with("---") => {
                        paint(color, "1", line)
                    }
                    Some('+') => paint(color, "32", line),
                    Some('-') => paint(color, "31", line),
                    Some('@') => paint(color, "36", line),
                    _ => line.to_owned(),
                };
                out.push_str(&format!("  {painted}\n"));
            }
        }
    }
    if !plan.notes.is_empty() {
        out.push('\n');
        for note in &plan.notes {
            out.push_str(&format!("note: {note}\n"));
        }
    }
    if !plan.commands.is_empty() {
        out.push_str("\nThen:\n");
        for c in &plan.commands {
            out.push_str(&format!("  {}\n      {}\n", c.command, c.why));
        }
    }
    out
}

#[allow(clippy::let_underscore_must_use)] // ratchet: branchyard-cli
fn cancelled(error: io::Error) -> Failure {
    if error.kind() == io::ErrorKind::Interrupted {
        let _ = cliclack::outro_cancel("Cancelled; nothing was written.");
        return Failure::Reported;
    }
    Failure::Io(error)
}

/// Ask one question; the answer as the engine takes it.
fn ask(q: &Question) -> io::Result<Value> {
    let prompt = format!("{}\n{}", q.prompt, paint(true, "2", &q.why));
    let text_input = |q: &Question| -> io::Result<Value> {
        let checker = q.clone();
        let mut input = cliclack::input(format!("{} ({})", q.header, q.prompt))
            .required(!q.optional)
            .validate(move |text: &String| {
                checker.normalize(&Value::String(text.clone())).map(|_| ())
            });
        if let Some(default) = display(&q.default) {
            input = input.default_input(&default);
        }
        let text: String = input.interact()?;
        Ok(Value::String(text))
    };
    match widget(q) {
        Widget::Confirm { initial } => Ok(Value::Bool(
            cliclack::confirm(prompt)
                .initial_value(initial)
                .interact()?,
        )),
        Widget::Input { .. } => text_input(q),
        Widget::Select {
            items,
            initial,
            other,
        } => {
            let mut select = cliclack::select(prompt).initial_value(initial);
            for (i, (_, label, hint)) in items.iter().enumerate() {
                select = select.item(i, label, hint);
            }
            if other {
                select = select.item(items.len(), OTHER, "anything the question accepts");
            }
            let chosen = select.interact()?;
            // The label, as a harness sends it: a choice whose value is
            // null means "skip", where a null answer would mean "default".
            match items.get(chosen) {
                Some((_, label, _)) => Ok(Value::String(label.clone())),
                None => text_input(q),
            }
        }
        Widget::MultiSelect {
            items,
            initial,
            other,
        } => {
            let mut select = cliclack::multiselect(prompt)
                .initial_values(initial)
                .required(false);
            for (i, (_, label, hint)) in items.iter().enumerate() {
                select = select.item(i, label, hint);
            }
            let chosen = select.interact()?;
            let mut values: Vec<Value> = chosen
                .iter()
                .filter_map(|i| items.get(*i))
                .map(|(_, label, _)| Value::String(label.clone()))
                .collect();
            if other {
                let more: String =
                    cliclack::input("Any others? (comma-separated names; leave empty for none)")
                        .required(false)
                        .interact()?;
                values.extend(
                    branchyard_setup::interview::split_list(&more)
                        .into_iter()
                        .map(Value::String),
                );
            }
            Ok(Value::Array(values))
        }
    }
}

/// The wizard: detect, ask batch by batch, review, confirm, write.
pub fn run(topic: Option<Topic>, defaults: bool) -> Outcome {
    cliclack::intro(paint(true, "1;7", " by init ")).map_err(Failure::Io)?;
    let topic = match topic {
        Some(topic) => topic,
        None => {
            let mut select = cliclack::select("What would you like to set up?");
            for t in Topic::ALL {
                select = select.item(t, t.title(), t.summary());
            }
            select.interact().map_err(cancelled)?
        }
    };
    let cwd = std::env::current_dir()?;
    let spinner = cliclack::spinner();
    spinner.start("Looking at this machine and repository…");
    let host = HostProbe::new(&cwd);
    let root = host.root_path().to_path_buf();
    let probe = CachedProbe::new(host);
    let facts = Facts::gather(&probe);
    spinner.stop("Detected");
    let lines: Vec<String> = facts
        .lines()
        .iter()
        .map(|f| format!("{}: {}", f.label, f.value))
        .collect();
    cliclack::note(topic.title(), lines.join("\n")).map_err(Failure::Io)?;
    let mut raw: BTreeMap<String, Value> = BTreeMap::new();
    let response = loop {
        let response = branchyard_setup::next(
            topic,
            &probe,
            &raw,
            defaults,
            &mut RandomEntropy,
            &HostValidator,
        );
        for error in &response.errors {
            cliclack::log::warning(format!("{}: {}", error.id, error.message))
                .map_err(Failure::Io)?;
        }
        if response.done {
            break response;
        }
        for q in &response.questions {
            let value = ask(q).map_err(cancelled)?;
            raw.insert(q.id.clone(), value);
        }
    };
    let Some(plan) = response.plan else {
        return Err(Failure::Message("the setup finished without a plan".into()));
    };
    cliclack::note("Review", review_text(&plan, true)).map_err(Failure::Io)?;
    if !plan.valid {
        cliclack::outro_cancel("The plan failed validation; nothing was written. Run by init again to change the answers.")
            .map_err(Failure::Io)?;
        return Err(Failure::Reported);
    }
    let writes = plan.files.iter().filter(|f| f.writes()).count();
    if writes == 0 {
        cliclack::outro("Everything is already as planned; nothing to write.")
            .map_err(Failure::Io)?;
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
        .interact()
        .map_err(cancelled)?;
        if !force {
            cliclack::outro_cancel("Nothing was written.").map_err(Failure::Io)?;
            return Ok(());
        }
    }
    let go = cliclack::confirm(format!("Write {writes} file(s)?"))
        .initial_value(true)
        .interact()
        .map_err(cancelled)?;
    if !go {
        cliclack::outro_cancel("Nothing was written.").map_err(Failure::Io)?;
        return Ok(());
    }
    match setup_io::apply(&plan, &root, force) {
        Ok(applied) => {
            for path in &applied.written {
                cliclack::log::success(format!("wrote {path}")).map_err(Failure::Io)?;
            }
            let next: Vec<String> = applied
                .commands
                .iter()
                .map(|c| format!("{}  # {}", c.command, c.why))
                .collect();
            cliclack::outro_note("Next", next.join("\n")).map_err(Failure::Io)?;
            Ok(())
        }
        Err(Refusal::Invalid(problems) | Refusal::WouldOverwrite(problems)) => {
            cliclack::outro_cancel(problems.join("\n")).map_err(Failure::Io)?;
            Err(Failure::Reported)
        }
        Err(Refusal::Io(message)) => Err(Failure::Message(message)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard_setup::probe::{CountingEntropy, FakeProbe};
    use branchyard_setup::BuiltinValidator;
    use serde_json::json;

    fn first_questions(topic: Topic) -> Vec<Question> {
        branchyard_setup::next(
            topic,
            &FakeProbe::typical(),
            &BTreeMap::new(),
            false,
            &mut CountingEntropy(0),
            &BuiltinValidator,
        )
        .questions
    }

    #[test]
    fn questions_map_onto_widgets_with_their_defaults() {
        let qs = first_questions(Topic::Project);
        let harness = qs.iter().find(|q| q.id == "harness").unwrap();
        match widget(harness) {
            Widget::Select {
                items,
                initial,
                other,
            } => {
                assert_eq!(items[initial].0, json!("claude-code"));
                assert!(items.len() > 4, "the wizard lists more_choices too");
                assert!(!other);
            }
            other => panic!("{other:?}"),
        }
        // The model's choices depend on the harness, so it comes a batch later.
        assert!(!qs.iter().any(|q| q.id == "model"));
        let facts = Facts::gather(&FakeProbe::typical());
        let answered = BTreeMap::from([("harness".to_owned(), json!("claude-code"))]);
        let all = branchyard_setup::questions(Topic::Project, &facts, &answered);
        let model = all.iter().find(|q| q.id == "model").unwrap();
        assert!(matches!(widget(model), Widget::Select { other: true, .. }));
        let server = first_questions(Topic::Server);
        let listen = server.iter().find(|q| q.id == "listen").unwrap();
        assert!(matches!(
            widget(listen),
            Widget::Select {
                initial: 0,
                other: true,
                ..
            }
        ));
        let facts = Facts::gather(&FakeProbe::typical());
        let scopes = branchyard_setup::questions(Topic::Server, &facts, &BTreeMap::new())
            .into_iter()
            .find(|q| q.id == "scopes");
        match widget(&scopes.unwrap()) {
            Widget::MultiSelect { initial, .. } => assert_eq!(initial, [0, 1, 2, 3]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_review_shows_diffs_and_verdicts_but_never_a_secret() {
        let response = branchyard_setup::next(
            Topic::Server,
            &FakeProbe::typical(),
            &BTreeMap::new(),
            true,
            &mut CountingEntropy(0),
            &BuiltinValidator,
        );
        let plan = response.plan.unwrap();
        let text = review_text(&plan, false);
        assert!(
            text.contains("create .branchyard/tokens/admin.token (mode 0600)"),
            "{text}"
        );
        assert!(text.contains("its content is never shown"));
        assert!(text.contains("+++ b/.branchyard/server.json"));
        assert!(text.contains("by serve --config .branchyard/server.json --check"));
        assert!(!text.contains("test-token-"), "{text}");
        assert!(!text.contains('\x1b'), "no color when asked for none");
        assert!(review_text(&plan, true).contains("\x1b[32m"));
    }
}
