//! Review before landing: a reviewer, on another harness and ideally
//! another vendor's, reads a child's work before the engine lands it.
//!
//! A child spawned with `review` and `land` (`by spawn --land --review
//! codex`, a plan's `"review": {"harness": "codex"}`) is reviewed each time
//! it is ready to land with a candidate no reviewer has approved
//! ([`crate::land`]). The engine spawns the reviewer as a child of the same
//! parent, from the child's candidate, with one turn, no delegation and
//! every tool request denied, so its cost is in the parent's tree and
//! within its limits. Its prompt holds the child's task, its diff, its
//! final message and the rubric; its answer, one JSON object, approves the
//! work or lists what to change. A child asked for changes is sent them as
//! a turn to fix its work, as for a check that failed on the merge. The
//! reviewer is discarded after its verdict is read, its log kept.
//!
//! A review that cannot be done (the reviewer's spawn is refused, its turn
//! fails or times out, or its answer has no verdict) leaves the child for
//! its parent: a gate that cannot be read does not open.

use std::collections::BTreeSet;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::delegation::Delegate;
use crate::state::Record;
use crate::{git, Budget, Error, Spawn, Yard};

/// Characters of a child's diff a reviewer's prompt quotes.
pub const DIFF_SHOWN: usize = crate::judge::DIFF_MAX;

/// Characters of a child's final message a reviewer's prompt quotes.
const MESSAGE_SHOWN: usize = 2000;

/// How long the engine waits for a reviewer's turn before it gives up.
const TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// A reviewer for a child's work before the engine lands it: a harness its
/// parent may delegate to, ideally another vendor's than the child's.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ReviewSpec {
    /// The reviewer's harness or profile ID, such as `codex`.
    pub harness: String,
    /// The model the reviewer's harness runs; unset, its default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// What to look for, added to the reviewer's prompt after the default
    /// rubric.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rubric: Option<String>,
    /// Each review's cost limit, from its parent's budget; required when
    /// the parent has a cost limit, as for any child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_usd: Option<f64>,
}

impl ReviewSpec {
    /// Refuse a reviewer that cannot be spawned as asked, before the child
    /// it would review is created.
    pub(crate) fn check(&self, land: bool, cost_limited: bool) -> Result<(), Error> {
        if !land {
            return Err(Error::Denied(
                "a review gates landing: give the child land too, or review its work yourself \
                 before you integrate it"
                    .into(),
            ));
        }
        if self.harness.trim().is_empty() {
            return Err(Error::Denied(
                "a review needs the reviewer's harness".into(),
            ));
        }
        match self.budget_usd {
            Some(usd) if !(usd.is_finite() && usd > 0.0) => Err(Error::Denied(format!(
                "a review's budget_usd must be more than 0, not {usd}"
            ))),
            None if cost_limited => Err(Error::Denied(
                "give the review a budget_usd: you have a cost limit, and each review is a child \
                 of yours that spends from it"
                    .into(),
            )),
            _ => Ok(()),
        }
    }
}

/// What a reviewer answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The work may land.
    Approve,
    /// What to change before it lands, one finding each.
    Changes(Vec<String>),
}

/// The reviewer `spec` of `child`, if its parent gave it one.
pub(crate) fn of(child: &Record) -> Option<&ReviewSpec> {
    child.grant.as_ref().and_then(|grant| grant.review.as_ref())
}

/// Spawn a reviewer of `child`'s candidate under `parent`, wait for its
/// turn, and read its verdict; the reviewer is discarded after. Returns
/// the verdict and the reviewer's branch.
pub(crate) fn run(
    yard: &Yard,
    delegate: &Delegate,
    parent: &Record,
    child: &Record,
    spec: &ReviewSpec,
) -> Result<(Verdict, String), Error> {
    let name = &child.info.name;
    let candidate = child
        .info
        .candidate
        .as_ref()
        .ok_or_else(|| Error::NoCandidate(name.clone()))?;
    let diff = git::diff(yard.root(), &child.info.base, &candidate.commit)?;
    let message = crate::judge::message(&yard.branch(name)?.events()?);
    let text = prompt(child, &parent.info.git_branch, &diff, &message, spec);
    let reviewer = crate::names::plan_one(
        &yard.store(),
        yard.root(),
        None,
        &format!("review {name}"),
        &BTreeSet::new(),
    )?;
    delegate.spawn(Spawn {
        prompt: text,
        harness: Some(spec.harness.clone()),
        name: Some(reviewer.clone()),
        base: Some(candidate.commit.clone()),
        budget: Budget {
            max_usd: spec.budget_usd,
            max_turns: Some(1),
            ..Budget::default()
        },
        max_depth: Some(0),
        deny: vec!["*".into()],
        model: spec.model.clone(),
        why: Some(format!("review of {name} before it lands")),
        ..Spawn::default()
    })?;
    let answered = match delegate.wait(&reviewer, TIMEOUT) {
        Ok(inspection) => match inspection.status {
            crate::BranchStatus::Ready | crate::BranchStatus::NoChanges => Ok(parse(
                &crate::judge::message(&yard.branch(&reviewer)?.events()?),
            )),
            other => Err(Error::Harness(format!(
                "the reviewer {reviewer} ended {}",
                crate::wake::status_text(&other)
            ))),
        },
        Err(Error::Running(_)) => {
            branchyard_support::best_effort("stop the reviewer", delegate.cancel(&reviewer));
            Err(Error::Harness(format!(
                "the reviewer {reviewer} did not finish in {} minutes",
                TIMEOUT.as_secs() / 60
            )))
        }
        Err(error) => Err(error),
    };
    let reason = match &answered {
        Ok(Ok(Verdict::Approve)) => format!("reviewed {name}: approved"),
        Ok(Ok(Verdict::Changes(found))) => {
            format!("reviewed {name}: asked for {} changes", found.len())
        }
        Ok(Err(_)) | Err(_) => format!("reviewed {name}: no verdict"),
    };
    branchyard_support::best_effort(
        "set the reviewer aside",
        delegate.discard(&reviewer, Some(&reason)),
    );
    match answered? {
        Ok(verdict) => Ok((verdict, reviewer)),
        Err(why) => Err(Error::Harness(format!(
            "the reviewer {reviewer} gave no verdict: {why}"
        ))),
    }
}

/// The reviewer's prompt: the child's task, its diff, its final message,
/// the rubric and the answer's form.
pub(crate) fn prompt(
    child: &Record,
    target: &str,
    diff: &str,
    message: &str,
    spec: &ReviewSpec,
) -> String {
    let name = &child.info.name;
    let mut text = format!(
        "<branchyard-review>\n\
         You are reviewing {name}'s work before it is integrated into its parent's branch \
         {target}. Read it and judge it; change nothing, since nothing you write is kept.\n\n\
         ## Its task\n\n{}\n\n## Its changes\n\n```diff\n{}\n```\n",
        child.info.prompt.trim(),
        cut_tail(
            diff.trim_end(),
            DIFF_SHOWN,
            "the rest of the diff is not shown"
        )
    );
    if !message.trim().is_empty() {
        text.push_str(&format!(
            "\n## Its final message\n\n{}\n",
            cut_front(message.trim(), MESSAGE_SHOWN)
        ));
    }
    text.push_str(
        "\n## Rubric\n\n\
         Approve when the change does what its task asks, is correct, and is safe to merge: \
         nothing unrelated, nothing broken, nothing the task needs left out. Ask for changes \
         only for what must change before it lands, each finding one concrete thing to change \
         and where. Its final message may answer an earlier finding; weigh it.\n",
    );
    if let Some(rubric) = spec.rubric.as_deref().filter(|r| !r.trim().is_empty()) {
        text.push('\n');
        text.push_str(rubric.trim());
        text.push('\n');
    }
    text.push_str(
        "\n## Your answer\n\n\
         Answer with one JSON object and nothing after it: {\"approve\": true} to let it land, \
         or {\"approve\": false, \"findings\": [\"what to change, and where\", ...]}.\n\
         </branchyard-review>",
    );
    text
}

/// The verdict in a reviewer's answer: the last JSON object in it with a
/// boolean `approve`. A refusal with no findings is not a verdict.
pub(crate) fn parse(text: &str) -> Result<Verdict, String> {
    let mut found = None;
    for (start, _) in text.match_indices('{') {
        let mut values = serde_json::Deserializer::from_str(&text[start..]).into_iter::<Value>();
        if let Some(Ok(value)) = values.next() {
            if value.get("approve").is_some_and(Value::is_boolean) {
                found = Some(value);
            }
        }
    }
    let value =
        found.ok_or_else(|| "its answer has no JSON object with a boolean approve".to_owned())?;
    if value["approve"] == Value::Bool(true) {
        return Ok(Verdict::Approve);
    }
    let findings: Vec<String> = value["findings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .map(str::to_owned)
        .collect();
    match findings.is_empty() {
        true => Err("it asked for changes but named none".to_owned()),
        false => Ok(Verdict::Changes(findings)),
    }
}

/// `text` cut to its first `max` characters, saying what was left out.
fn cut_tail(text: &str, max: usize, left_out: &str) -> String {
    match text.chars().count() > max {
        true => format!(
            "{}\n… ({left_out})",
            text.chars().take(max).collect::<String>()
        ),
        false => text.to_owned(),
    }
}

/// `text` cut to its last `max` characters.
fn cut_front(text: &str, max: usize) -> String {
    let count = text.chars().count();
    match count > max {
        true => format!("…{}", text.chars().skip(count - max).collect::<String>()),
        false => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_verdict_is_the_last_object_with_a_boolean_approve() {
        assert_eq!(parse(r#"{"approve": true}"#), Ok(Verdict::Approve));
        let fenced = "I looked.\n```json\n{\"approve\": false, \"findings\": \
                      [\" Handle the empty input in src/lexer.rs \", \"\"]}\n```";
        assert_eq!(
            parse(fenced),
            Ok(Verdict::Changes(vec![
                "Handle the empty input in src/lexer.rs".into()
            ]))
        );
        // A later verdict replaces an earlier one; other objects are not one.
        let two = r#"{"approve": false, "findings": ["x"]} then {"note": 1} {"approve": true}"#;
        assert_eq!(parse(two), Ok(Verdict::Approve));
        assert!(parse("looks fine to me").is_err());
        assert!(parse(r#"{"approve": "yes"}"#).is_err());
        assert_eq!(
            parse(r#"{"approve": false}"#),
            Err("it asked for changes but named none".into())
        );
    }

    #[test]
    fn a_review_needs_land_a_harness_and_a_budget_under_a_cost_limit() {
        let spec = ReviewSpec {
            harness: "codex".into(),
            ..ReviewSpec::default()
        };
        assert!(spec.check(true, false).is_ok());
        let refused = |spec: &ReviewSpec, land, limited| match spec.check(land, limited) {
            Err(Error::Denied(why)) => why,
            other => panic!("{other:?}"),
        };
        assert!(refused(&spec, false, false).contains("gates landing"));
        assert!(refused(&spec, true, true).contains("budget_usd"));
        let free = ReviewSpec {
            budget_usd: Some(0.0),
            ..spec.clone()
        };
        assert!(refused(&free, true, false).contains("more than 0"));
        let nobody = ReviewSpec {
            harness: " ".into(),
            ..spec
        };
        assert!(refused(&nobody, true, false).contains("harness"));
    }

    #[test]
    fn long_texts_are_cut_and_say_so() {
        let long = "x".repeat(10);
        assert_eq!(cut_tail(&long, 4, "cut"), "xxxx\n… (cut)");
        assert_eq!(cut_front(&long, 4), "…xxxx");
        assert_eq!(cut_tail("short", 10, "cut"), "short");
    }
}
