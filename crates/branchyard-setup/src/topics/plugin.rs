//! `plugin`: the Branchyard skills, installed into a harness's skill
//! directory byte for byte, or the command that loads the whole plugin.

use serde_json::json;

use super::{join, list, text};
use crate::interview::{Answers, Choice, Kind, Question};
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
        .when(crate::interview::Condition::equals("host", "plugin-dir").negate()),
    ]
}

pub fn plan(facts: &Facts, answers: &Answers, probe: &dyn Probe) -> Plan {
    let host = text(answers, "host").unwrap_or_else(|| "claude-project".into());
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
