//! `plugin`: the Branchyard skills, installed into a harness's skill
//! directory byte for byte, or the command that loads the whole plugin, or
//! each chosen harness's install manifest (`plugins/branchyard/install`):
//! hooks, commands and skills written or merged into its own files.

use serde_json::json;

use super::{join, list, text};
use crate::install;
use crate::interview::{Answers, Choice, Condition, Kind, Question};
use crate::plan::{ArtifactKind, Plan, PlannedFile};
use crate::probe::{Facts, Probe};
use crate::skills::SKILLS;
use crate::Topic;

/// Where each host keeps skills.
fn destination(facts: &Facts, host: &str) -> Option<String> {
    let home = facts.home.clone().unwrap_or_else(|| "~".into());
    match host {
        "claude-project" => Some(".claude/skills".into()),
        "claude-user" => Some(format!("{home}/.claude/skills")),
        "codex-user" => Some(format!("{home}/.codex/skills")),
        _ => None,
    }
}

pub fn questions(facts: &Facts, _answers: &Answers) -> Vec<Question> {
    let codex_only = facts.installed().any(|h| h.harness == "codex")
        && !facts.installed().any(|h| h.harness == "claude-code");
    let default = match codex_only {
        true => "codex-user",
        false => "claude-project",
    };
    let mut hosts = vec![
        Choice::new(
            "claude-project",
            "Claude Code: this repository",
            ".claude/skills here; shared through git",
        ),
        Choice::new(
            "claude-user",
            "Claude Code: just me",
            "~/.claude/skills; every repository",
        ),
        Choice::new("codex-user", "Codex", "~/.codex/skills"),
    ];
    hosts.push(Choice::new(
        "plugin-dir",
        "Claude Code plugin",
        match &facts.plugin_dir {
            Some(dir) => format!("load {dir} with claude --plugin-dir; writes nothing"),
            None => {
                "load an extracted plugin archive with claude --plugin-dir; writes nothing".into()
            }
        },
    ));
    hosts.push(Choice::new(
        "harnesses",
        "Hooks for each harness",
        match facts.install_detected.is_empty() {
            true => "hooks, commands and skills per harness; none detected on PATH".to_owned(),
            false => format!(
                "hooks, commands and skills per harness; detected: {}",
                facts.install_detected.join(", ")
            ),
        },
    ));
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
    vec![
        Question::new(
            "host",
            Kind::Select,
            "Install to",
            "Where should the Branchyard skills go?",
            "The skills teach a harness to set Branchyard up (setup) and to delegate to child branches (delegate).",
        )
        .default(default)
        .choices(hosts),
        Question::new(
            "skills",
            Kind::Multiselect,
            "Skills",
            "Which skills?",
            "setup drives by init; delegate is for harnesses running on a Branchyard branch.",
        )
        .default(json!(["setup", "delegate"]))
        .choices(vec![
            Choice::new("setup", "setup", "interview the user and write validated configuration"),
            Choice::new("delegate", "delegate", "spawn, watch and integrate child branches"),
        ])
        .when(
            Condition::In {
                id: "host".into(),
                values: vec![json!("plugin-dir"), json!("harnesses")],
            }
            .negate(),
        ),
        Question::new(
            "harnesses",
            Kind::Multiselect,
            "Harnesses",
            "Which harnesses should get Branchyard's hooks?",
            "Each gets what plugins/branchyard/install/ID.toml lists. Merges into its settings only add; the review shows every diff before anything is written.",
        )
        .default(json!(facts.install_detected))
        .choices(harnesses)
        .when(Condition::equals("host", "harnesses")),
    ]
}

/// The plan for `host = harnesses`: each chosen harness's manifest.
fn harnesses_plan(facts: &Facts, answers: &Answers, probe: &dyn Probe) -> Plan {
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
    }
    plan.files = install::planned_files(&picked, facts, probe);
    for manifest in &picked {
        let label = crate::probe::harness_label(&manifest.harness);
        let targets: Vec<String> = manifest
            .files
            .iter()
            .map(|f| install::expand(&f.to, facts))
            .collect();
        let count = plan
            .files
            .iter()
            .filter(|p| {
                targets
                    .iter()
                    .any(|t| p.path == *t || p.path.starts_with(&format!("{t}/")))
            })
            .count();
        plan.summary.push(format!(
            "{label}: {} ({count} file(s))",
            manifest.provides.join(", ")
        ));
        for note in &manifest.notes {
            plan.notes.push(format!("{label}: {}", note.text));
        }
    }
    plan.notes.push(
        "Each hook runs by hook --harness ID EVENT and is silent without by on PATH (docs/harness-hooks.md).".into(),
    );
    plan.command(
        "by init plugin",
        "List every harness Branchyard installs for, and which are detected here.",
    );
    plan
}

pub fn plan(facts: &Facts, answers: &Answers, probe: &dyn Probe) -> Plan {
    let host = text(answers, "host").unwrap_or_else(|| "claude-project".into());
    if host == "harnesses" {
        return harnesses_plan(facts, answers, probe);
    }
    let mut plan = Plan::new(Topic::Plugin);
    let Some(dest) = destination(facts, &host) else {
        let dir = facts
            .plugin_dir
            .clone()
            .unwrap_or_else(|| "path/to/branchyard-plugin".into());
        plan.summary.push(format!(
            "Load the Branchyard plugin from {dir}; nothing is written"
        ));
        plan.command(
            format!("claude --plugin-dir {dir}"),
            "Start Claude Code with the plugin: /branchyard:setup and both skills.",
        );
        plan.notes.push("Build a plugin archive with python3 tools/package.py --output dist (docs/distribution.md).".into());
        return plan;
    };
    let chosen = list(answers, "skills");
    for skill in SKILLS.iter().filter(|s| chosen.iter().any(|c| c == s.name)) {
        for (path, body) in skill.files {
            let target = join(&join(&dest, skill.name), path);
            let kind = match path.ends_with(".md") {
                true => ArtifactKind::Skill,
                false => ArtifactKind::Text,
            };
            plan.files.push(PlannedFile::new(
                &target,
                kind,
                0o644,
                (*body).to_owned(),
                probe.read(&target),
            ));
        }
    }
    plan.summary
        .push(format!("Install {} into {dest}", chosen.join(" and ")));
    if let Some(dir) = &facts.plugin_dir {
        plan.notes.push(format!(
            "The same copy the shipped installer makes: python3 {dir}/scripts/install_skill.py --destination {dest} --skill NAME --apply"
        ));
    }
    let restart = match host.as_str() {
        "codex-user" => "Restart Codex, then ask it to \"set up Branchyard\".",
        _ => "Restart Claude Code, then ask it to \"set up Branchyard\" or run /setup.",
    };
    plan.command("by init --json", restart);
    plan
}
