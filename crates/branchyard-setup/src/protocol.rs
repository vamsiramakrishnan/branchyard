//! The machine protocol's JSON documents: `by init TOPIC --json --next`
//! and `--dry-run --json` print a [`Response`], `--apply --json` an
//! [`Applied`], `by init --json` a [`TopicList`]. Their JSON Schema is
//! `schema/setup.protocol.json`. Maps are ordered and questions keep the
//! topic's order, so the same answers always print the same bytes.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::interview::{AnswerError, Answers, Question, State};
use crate::plan::{FollowUp, Plan};
use crate::probe::{Fact, Facts};
use crate::{Topic, PROTOCOL};

/// The next questions, or the plan once none remain.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Response {
    /// Always `branchyard.setup/v1`.
    pub protocol: String,
    pub topic: Topic,
    /// No question remains; `plan` is set.
    pub done: bool,
    /// What was detected, to show before asking.
    pub facts: Vec<Fact>,
    /// The answers accepted so far, normalized (labels mapped to values,
    /// text to numbers and booleans); `null` for a question that does not
    /// apply. Send them back, with the new ones, for the next step.
    pub answers: Answers,
    /// Answers that were refused; their questions are asked again.
    pub errors: Vec<AnswerError>,
    /// Answer ids no question has; ignored.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unknown_answers: Vec<String>,
    /// The next batch: at most four questions, none depending on another
    /// in the batch. Empty when `done`.
    pub questions: Vec<Question>,
    /// Questions left after this batch, as far as is known now.
    pub remaining: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<Plan>,
}

impl Response {
    pub fn new(topic: Topic, facts: &Facts, state: State, plan: Option<Plan>) -> Response {
        let batch = state.batch();
        Response {
            protocol: PROTOCOL.into(),
            topic,
            done: plan.is_some(),
            facts: facts.lines(),
            remaining: state.pending.len() - batch.len() + state.deferred,
            answers: state.answers,
            errors: state.errors,
            unknown_answers: state.unknown,
            questions: batch,
            plan,
        }
    }
}

/// What `--apply` did.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Applied {
    pub protocol: String,
    pub topic: Topic,
    /// Files written, in order.
    pub written: Vec<String>,
    /// Files already exactly as planned, or secrets kept.
    pub unchanged: Vec<String>,
    /// Run these next, in order.
    pub commands: Vec<FollowUp>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TopicInfo {
    pub id: String,
    pub title: String,
    pub summary: String,
    /// The first step of its interview.
    pub start: String,
}

/// `by init --json`: the topics.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TopicList {
    pub protocol: String,
    pub topics: Vec<TopicInfo>,
}

impl TopicList {
    pub fn new() -> TopicList {
        TopicList {
            protocol: PROTOCOL.into(),
            topics: Topic::ALL
                .iter()
                .map(|t| TopicInfo {
                    id: t.id().into(),
                    title: t.title().into(),
                    summary: t.summary().into(),
                    start: format!("by init {} --json --next", t.id()),
                })
                .collect(),
        }
    }
}

impl Default for TopicList {
    fn default() -> Self {
        TopicList::new()
    }
}

/// A refusal, as every `by --json` command prints it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ErrorResponse {
    pub error: ErrorBody,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ErrorBody {
    /// `incomplete`, `invalid_plan`, `would_overwrite`, `invalid_answers`,
    /// `usage`, `io`.
    pub kind: String,
    pub message: String,
    /// For `would_overwrite`: the files that differ.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}
