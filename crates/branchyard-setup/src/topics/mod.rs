//! One module per topic: its questions, in order, and its plan.

pub mod deploy;
pub mod plugin;
pub mod project;
pub mod rig;
pub mod server;

use serde_json::Value;

use crate::interview::{Answers, Choice, Kind, Question};
use crate::probe::{harness_label, unapproved_tools, Facts};

pub(crate) fn text(answers: &Answers, id: &str) -> Option<String> {
    answers.get(id).and_then(Value::as_str).map(str::to_owned)
}

pub(crate) fn float(answers: &Answers, id: &str) -> Option<f64> {
    answers.get(id).and_then(Value::as_f64)
}

pub(crate) fn flag(answers: &Answers, id: &str) -> bool {
    answers.get(id).and_then(Value::as_bool).unwrap_or(false)
}

pub(crate) fn list(answers: &Answers, id: &str) -> Vec<String> {
    match answers.get(id) {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// A harness choice for every profile's harness, installed ones first.
pub(crate) fn harness_question(
    id: &str,
    header: &str,
    prompt: &str,
    why: &str,
    facts: &Facts,
    default: &str,
) -> Question {
    let mut harnesses: Vec<&str> = Vec::new();
    for profile in branchyard_harness::profiles::PROFILES {
        if !harnesses.contains(&profile.harness) {
            harnesses.push(profile.harness);
        }
    }
    let fact = |h: &str| facts.harnesses.iter().find(|f| f.harness == h);
    let installed = |h: &str| fact(h).is_some_and(|f| f.installed);
    harnesses.sort_by_key(|h| !installed(h));
    let choices = harnesses
        .into_iter()
        .map(|h| {
            let mut description = match fact(h) {
                Some(f) if f.installed => match &f.version {
                    Some(v) if !v.is_empty() => format!("installed: {v}"),
                    _ => "installed".to_owned(),
                },
                _ => "not found on PATH".to_owned(),
            };
            if let Some(q) = fact(h).and_then(|f| f.qualification.as_ref()) {
                description.push_str(&format!("; qualified {q}"));
            }
            if unapproved_tools(h) {
                description.push_str("; cannot route permission requests");
            }
            Choice::new(h, harness_label(h), description)
        })
        .collect();
    Question::new(id, Kind::Select, header, prompt, why)
        .default(default)
        .choices(choices)
}

/// A path relative to a directory `depth` components below the root, back
/// to the root: `..` per component, or `.`.
pub(crate) fn back_to_root(relative_file: &str) -> String {
    let depth = relative_file
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .count()
        .saturating_sub(1);
    match depth {
        0 => ".".into(),
        n => vec![".."; n].join("/"),
    }
}

/// The directory of a path, `.` for none.
pub(crate) fn parent(path: &str) -> String {
    match path.rsplit_once('/') {
        Some(("", _)) => "/".into(),
        Some((dir, _)) => dir.into(),
        None => ".".into(),
    }
}

/// `dir/name`, without a leading `./`.
pub(crate) fn join(dir: &str, name: &str) -> String {
    match dir {
        "." | "" => name.into(),
        dir => format!("{}/{name}", dir.trim_end_matches('/')),
    }
}

/// Secrets each harness's provisioning reads, in order of preference.
pub(crate) fn harness_secrets(harness: &str) -> &'static [&'static str] {
    match harness {
        "claude-code" => &["ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN"],
        "codex" => &["OPENAI_API_KEY", "CODEX_API_KEY"],
        "gemini-cli" | "antigravity" => &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        "opencode" | "hermes" => &["ANTHROPIC_API_KEY", "OPENAI_API_KEY"],
        "github-copilot" => &["COPILOT_GITHUB_TOKEN", "GH_TOKEN"],
        _ => &[],
    }
}

/// Choices of secret names: those for `harnesses`, then any other known
/// credential variable that is set; each says whether it is set here.
pub(crate) fn secret_choices(facts: &Facts, harnesses: &[String]) -> (Vec<Choice>, Vec<String>) {
    let mut names: Vec<String> = Vec::new();
    for harness in harnesses {
        for name in harness_secrets(harness) {
            if !names.iter().any(|n| n == name) {
                names.push((*name).to_owned());
            }
        }
    }
    for name in &facts.variables {
        let credential = !name.starts_with("BRANCHYARD_") && name != "DATABASE_URL";
        if credential && !names.contains(name) {
            names.push(name.clone());
        }
    }
    // Suggested: the harnesses' own credentials that are set here; other
    // set variables are offered, not chosen.
    let set: Vec<String> = harnesses
        .iter()
        .flat_map(|h| harness_secrets(h).iter())
        .filter(|n| facts.variables.contains(**n))
        .map(|n| (*n).to_owned())
        .fold(Vec::new(), |mut acc, n| {
            if !acc.contains(&n) {
                acc.push(n);
            }
            acc
        });
    let choices = names
        .iter()
        .map(|name| {
            let here = match facts.variables.contains(name) {
                true => "set in this shell",
                false => "not set in this shell",
            };
            Choice::new(name.as_str(), name.as_str(), here)
        })
        .collect();
    (choices, set)
}

/// A `secret_ref` question for the secret `name`.
pub(crate) fn secret_ref_question(prefix: &str, name: &str, facts: &Facts, why: &str) -> Question {
    let set = facts.variables.contains(name);
    let file = format!(
        "@~/.config/branchyard/secrets/{}",
        name.to_ascii_lowercase()
    );
    Question::new(
        &format!("{prefix}{name}"),
        Kind::SecretRef,
        "Secret",
        &format!("Where is {name} read from?"),
        why,
    )
    .secret(name)
    .default(name)
    .choices(vec![
        Choice::new(
            name,
            format!("Variable {name}"),
            match set {
                true => "the variable of that name; set in this shell",
                false => "the variable of that name; not set in this shell",
            },
        ),
        Choice::new(
            file.as_str(),
            "A file",
            format!("{file} (0600); any @path works"),
        ),
    ])
    .rule(crate::interview::Rule::SecretRef)
}
