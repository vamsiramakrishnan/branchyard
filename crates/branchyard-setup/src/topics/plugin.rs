//! `plugin`: each chosen harness's install manifest
//! (`plugins/branchyard/install`): hooks, commands and skills written or
//! merged into its own files. One installer, whether the harnesses are
//! named (`by init plugin ID...`) or chosen in the interview.

use serde_json::json;

use super::list;
use crate::install;
use crate::interview::{Answers, Choice, Kind, Question};
use crate::plan::Plan;
use crate::probe::{harness_label, Facts, Probe};
use crate::Topic;

pub fn questions(facts: &Facts, _answers: &Answers) -> Vec<Question> {
    // Detected first (the listing's order), so they are among the
    // choices shown.
    let harnesses: Vec<Choice> = install::listing(facts)
        .map(|list| list.harnesses)
        .unwrap_or_default()
        .into_iter()
        .map(|h| {
            let found = match h.detected {
                true => format!("found on PATH ({})", h.binaries.join(" or ")),
                false => format!("{} not on PATH", h.binaries.join(" or ")),
            };
            Choice::new(
                h.harness.as_str(),
                h.label,
                format!("{found}; {}", h.provides.join(", ")),
            )
        })
        .collect();
    vec![Question::new(
        "harnesses",
        Kind::Multiselect,
        "Harnesses",
        "Which harnesses should get Branchyard's hooks, commands and skills?",
        "Each gets what plugins/branchyard/install/ID.toml lists. Merges into its settings only add; the review shows every diff before anything is written.",
    )
    .default(json!(facts.install_detected))
    .choices(harnesses)]
}

/// `path` with the home directory written `~`.
fn tilde(path: &str, home: Option<&str>) -> String {
    let rest = home
        .map(|h| h.trim_end_matches('/'))
        .filter(|h| !h.is_empty())
        .and_then(|h| path.strip_prefix(h))
        .filter(|rest| rest.starts_with('/'));
    match rest {
        Some(rest) => format!("~{rest}"),
        None => path.to_owned(),
    }
}

/// The plan for the chosen harnesses: each one's manifest.
pub fn plan(facts: &Facts, answers: &Answers, probe: &dyn Probe) -> Plan {
    let mut plan = Plan::new(Topic::Plugin);
    let manifests = match install::manifests() {
        Ok(all) => all,
        Err(e) => {
            plan.summary.push(format!(
                "The install manifests in this build are unreadable: {e}"
            ));
            return plan;
        }
    };
    let picked: Vec<install::Manifest> = list(answers, "harnesses")
        .iter()
        .filter_map(|id| manifests.iter().find(|m| &m.harness == id).cloned())
        .collect();
    if picked.is_empty() {
        plan.summary
            .push("No harness chosen; nothing to install".into());
        plan.command(
            "by init plugin",
            "List every harness Branchyard installs for, and which are detected here.",
        );
        return plan;
    }
    let installed = install::planned_install(&picked, facts, probe);
    plan.files = installed.files;
    for manifest in &picked {
        let label = harness_label(&manifest.harness);
        plan.summary.push(format!(
            "{label}: {}",
            install::summary(manifest, &installed.kept)
        ));
        // A setting the person already chose is left alone; the plan says
        // what to write to take Branchyard's instead.
        for kept in installed
            .kept
            .iter()
            .filter(|k| k.harness == manifest.harness && !k.same)
        {
            plan.notes.push(format!(
                "{label}: kept the \"{key}\" already in {path}. To use Branchyard's instead, \
                 set \"{key}\": {ours}",
                key = kept.key,
                path = tilde(&kept.path, facts.home.as_deref()),
                ours = kept.ours,
            ));
        }
        for note in &manifest.notes {
            plan.notes.push(format!("{label}: {}", note.text));
        }
    }
    plan.notes.push(
        "Each hook runs by hook --harness ID EVENT and is silent without by on PATH (docs/harness-hooks.md).".into(),
    );
    plan.command(
        "by setup",
        "Set this repository up too: a branchyard.toml, and what only you can do in each harness (`by init project` asks its questions one by one).",
    );
    plan
}
