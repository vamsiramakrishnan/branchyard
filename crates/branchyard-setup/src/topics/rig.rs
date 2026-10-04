//! `rig`: a rig spec with a lead seat and the seats it delegates to.

use serde_json::Value;

use super::{flag, float, harness_question, text};
use crate::config::{toml_number, toml_string};
use crate::interview::{Answers, Choice, Condition, Kind, Question, Rule};
use crate::plan::{ArtifactKind, Plan, PlannedFile};
use crate::probe::{unapproved_tools, Facts, Probe};
use crate::Topic;

#[allow(clippy::map_unwrap_or)] // ratchet: branchyard-setup
pub fn questions(facts: &Facts, answers: &Answers) -> Vec<Question> {
    let lead = facts.preferred_harness();
    let other = match lead.as_str() {
        "codex" => "claude-code".to_owned(),
        _ if facts.installed().any(|h| h.harness == "codex") => "codex".into(),
        _ => lead.clone(),
    };
    let name = text(answers, "name").unwrap_or_else(|| "team".into());
    let not_solo = Condition::equals("shape", "solo").negate();
    vec![
        Question::new(
            "name",
            Kind::Text,
            "Rig name",
            "What should the rig be called?",
            "It names the root branch and prefixes its children's names.",
        )
        .default("team")
        .choices(vec![
            Choice::new("team", "team", "a general-purpose team"),
            Choice::new("feature", "feature", "a feature team"),
        ])
        .rule(Rule::Name { max: 40 }),
        Question::new(
            "shape",
            Kind::Select,
            "Shape",
            "Which team shape?",
            "The lead's harness fills these seats with by spawn --seat; the file bounds what it may create.",
        )
        .default("review")
        .choices(vec![
            Choice::new("review", "Lead + implementer + reviewer", "two implementers may run; a reviewer that cannot edit"),
            Choice::new("pair", "Lead + implementer", "up to two implementers"),
            Choice::new("compare", "Two implementers", "the same task on two harnesses; keep the better"),
            Choice::new("solo", "Lead only", "one seat: limits and policy without delegation"),
        ]),
        harness_question(
            "lead.harness",
            "Lead",
            "Which harness plans and integrates as the lead?",
            "The root seat; it needs a harness that routes permission requests to delegate safely.",
            facts,
            &lead,
        ),
        Question::new(
            "budget_usd",
            Kind::Number,
            "Budget",
            "What may the whole rig spend, in dollars?",
            "The lead's budget; each child seat's budget is carved out of it so the plan checks.",
        )
        .default(6)
        .choices(vec![
            Choice::new(3, "$3", "small tasks"),
            Choice::new(6, "$6", "a typical feature"),
            Choice::new(20, "$20", "large work"),
        ])
        .rule(Rule::Min { value: 0.1 }),
        harness_question(
            "implementer.harness",
            "Implementer",
            "Which harness implements?",
            "The seat that makes the change and commits it on its branch.",
            facts,
            &other,
        )
        .when(not_solo.clone()),
        harness_question(
            "reviewer.harness",
            "Reviewer",
            "Which harness reviews?",
            "A reviewer denied every editing tool; a second harness catches different mistakes.",
            facts,
            &lead,
        )
        .when(Condition::equals("shape", "review")),
        harness_question(
            "second.harness",
            "Second",
            "Which harness takes the second attempt?",
            "Two implementer seats on different harnesses; the lead integrates the better one.",
            facts,
            &lead,
        )
        .when(Condition::equals("shape", "compare")),
        {
            let mut choices = Vec::new();
            if let Some(check) = &facts.suggested_check {
                choices.push(Choice::new(check.as_str(), check.as_str(), "run before a merge"));
            }
            choices.push(Choice::new(Value::Null, "No check", "integrate without a check"));
            Question::new(
                "check",
                Kind::Text,
                "Check",
                "Which command must pass before a seat's work is integrated?",
                "The lead's check; children keep it unless their seat sets one.",
            )
            .optional()
            .default(facts.suggested_check.clone().map(Value::from).unwrap_or(Value::Null))
            .choices(choices)
            .rule(Rule::CommandLine)
        },
        Question::new(
            "policy",
            Kind::Select,
            "Policy",
            "What should the lead's tool requests default to?",
            "Rules decide first; this answers the rest. Children may only add denials.",
        )
        .default("allow")
        .choices(vec![
            Choice::new("allow", "Allow", "allow what no rule denies; web access denied"),
            Choice::new("ask", "Ask", "ask on the terminal (local runs only)"),
            Choice::new("deny", "Deny", "deny what no rule allows"),
        ]),
        Question::new(
            "delegation_commands",
            Kind::Confirm,
            "Delegation",
            "Should the lead's own by spawn and by integrate run without asking?",
            "Allows only Branchyard's delegation commands, nothing else.",
        )
        .default(true)
        .choices(vec![
            Choice::new(true, "Yes", "the lead delegates without prompts"),
            Choice::new(false, "No", "each delegation command goes through the policy"),
        ])
        .when(not_solo),
        Question::new("path", Kind::Path, "File", "Where should the rig file go?", "by rig check and by rig run read it.")
            .default(format!("rigs/{name}.toml"))
            .choices(vec![Choice::new(format!("rigs/{name}.toml"), format!("rigs/{name}.toml"), "beside other rigs")])
            .when(Condition::truthy("name")),
    ]
}

/// Dollars rounded down to the cent, so children never exceed their parent.
fn cents(usd: f64) -> f64 {
    (usd * 100.0).floor() / 100.0
}

struct SeatText {
    name: &'static str,
    description: &'static str,
    harness: String,
    budget: String,
    extra: Vec<String>,
}

pub fn plan(facts: &Facts, answers: &Answers, probe: &dyn Probe) -> Plan {
    let name = text(answers, "name").unwrap_or_else(|| "team".into());
    let shape = text(answers, "shape").unwrap_or_else(|| "review".into());
    let path = text(answers, "path").unwrap_or_else(|| format!("rigs/{name}.toml"));
    let budget = float(answers, "budget_usd").unwrap_or(6.0);
    let lead = text(answers, "lead.harness").unwrap_or_else(|| facts.preferred_harness());
    let harness = |id: &str| text(answers, id).unwrap_or_else(|| lead.clone());
    let policy = text(answers, "policy").unwrap_or_else(|| "allow".into());
    let mut seats: Vec<SeatText> = Vec::new();
    let mut delegates: Vec<&str> = Vec::new();
    let budget_line = |usd: f64, turns: u32| {
        format!(
            "{{ max_usd = {}, max_turns = {turns} }}",
            toml_number(cents(usd))
        )
    };
    match shape.as_str() {
        "pair" => {
            delegates.push("implementer");
            seats.push(SeatText {
                name: "implementer",
                description: "Make one well-scoped change with tests, and leave it committed on your branch.",
                harness: harness("implementer.harness"),
                budget: budget_line(budget / 2.0, 10),
                extra: vec!["instances = 2".into()],
            });
        }
        "compare" => {
            delegates.extend(["first", "second"]);
            seats.push(SeatText {
                name: "first",
                description: "Attempt the task your lead gives you; commit a complete, tested change on your branch.",
                harness: harness("implementer.harness"),
                budget: budget_line(budget / 3.0, 10),
                extra: vec![],
            });
            seats.push(SeatText {
                name: "second",
                description: "Attempt the same task independently; commit a complete, tested change on your branch.",
                harness: harness("second.harness"),
                budget: budget_line(budget / 3.0, 10),
                extra: vec![],
            });
        }
        "solo" => {}
        _ => {
            delegates.extend(["implementer", "reviewer"]);
            seats.push(SeatText {
                name: "implementer",
                description: "Make one well-scoped change with tests, and leave it committed on your branch.",
                harness: harness("implementer.harness"),
                budget: budget_line(budget / 3.0, 10),
                extra: vec!["instances = 2".into()],
            });
            seats.push(SeatText {
                name: "reviewer",
                description:
                    "Review the candidate your lead names; report problems, do not edit files.",
                harness: harness("reviewer.harness"),
                budget: budget_line(budget / 6.0, 4),
                extra: vec![
                    "policy = { deny = [\"Edit\", \"Write\", \"MultiEdit\", \"NotebookEdit\"] }"
                        .into(),
                ],
            });
        }
    }
    let lead_description = match shape.as_str() {
        "solo" => "Do the task within your limits.",
        "compare" => "Give the same task to both implementers, compare their candidates, and integrate the better one.",
        _ => "Plan the change, delegate it, have it reviewed, and integrate what passes.",
    };
    let mut out = String::new();
    out.push_str(&format!(
        "# A rig written by `by init rig` ({shape}). Every field: docs/rigs.md.\n#\n"
    ));
    out.push_str(&format!(
        "#   by rig check {path}\n#   by rig run {path} \"<task>\"\n"
    ));
    out.push_str(&format!(
        "version = 1\nname = {}\nroot = \"lead\"\n",
        toml_string(&name)
    ));
    out.push_str(&format!(
        "description = {}\n",
        toml_string(lead_description)
    ));
    out.push_str("\n[seats.lead]\n");
    out.push_str(&format!(
        "description = {}\n",
        toml_string(lead_description)
    ));
    out.push_str(&format!("harness = {}\n", toml_string(&lead)));
    out.push_str(&format!(
        "budget = {{ max_usd = {}, max_turns = 20, max_minutes = 60 }}\n",
        toml_number(cents(budget))
    ));
    if let Some(check) = text(answers, "check") {
        out.push_str(&format!("check = {}\n", toml_string(&check)));
    }
    if !delegates.is_empty() {
        let list: Vec<String> = delegates.iter().map(|d| toml_string(d)).collect();
        out.push_str(&format!("delegates_to = [{}]\n", list.join(", ")));
    }
    out.push_str("\n[seats.lead.policy]\n");
    out.push_str(&format!("default = {}\n", toml_string(&policy)));
    out.push_str("deny = [\"WebFetch\", \"WebSearch\"]\n");
    if !delegates.is_empty() && flag(answers, "delegation_commands") {
        out.push_str("delegation_commands = true\n");
    }
    for seat in &seats {
        out.push_str(&format!("\n[seats.{}]\n", seat.name));
        out.push_str(&format!(
            "description = {}\n",
            toml_string(seat.description)
        ));
        out.push_str(&format!("harness = {}\n", toml_string(&seat.harness)));
        out.push_str(&format!("budget = {}\n", seat.budget));
        for line in &seat.extra {
            out.push_str(line);
            out.push('\n');
        }
    }
    let mut plan = Plan::new(Topic::Rig);
    let mut harnesses: Vec<&str> = vec![lead.as_str()];
    harnesses.extend(seats.iter().map(|s| s.harness.as_str()));
    plan.summary.push(format!(
        "Rig {name} ({shape}): lead on {lead}{}; ${} in all",
        seats
            .iter()
            .map(|s| format!(", {} on {}", s.name, s.harness))
            .collect::<String>(),
        toml_number(cents(budget))
    ));
    let unapproved: Vec<&str> = harnesses
        .iter()
        .copied()
        .filter(|h| unapproved_tools(h))
        .collect();
    if !unapproved.is_empty() {
        plan.notes.push(format!(
            "{} cannot route tool permission requests; by rig run needs --allow-unapproved-tools.",
            unapproved.join(", ")
        ));
    }
    if policy == "ask" {
        plan.notes
            .push("policy.default = \"ask\" is refused on a server (by --remote rig run).".into());
    }
    plan.files.push(PlannedFile::new(
        &path,
        ArtifactKind::Rig,
        0o644,
        out,
        probe.read(&path),
    ));
    plan.command(
        format!("by rig check {path}"),
        "Validate the rig and print its plan.",
    );
    plan.command(
        format!("by rig run {path} \"<describe the task>\""),
        "Start the lead; it fills the seats with by spawn --seat.",
    );
    plan
}
