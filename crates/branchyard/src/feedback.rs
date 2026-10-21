//! Feedback sent into a branch as a prompt (`by pr --watch`'s pull request
//! feedback, `by review`'s comments), recorded as data beside the prompt
//! as [`Activity::Feedback`](crate::Activity::Feedback): where each piece
//! came from, the file and line it is about, and its text.
//!
//! The prompt the harness reads is prose and may be reworded at will;
//! readers of the log, such as knowledge distillation (`docs/knowledge.md`),
//! read this record instead. It names its prompt by a digest of the
//! prompt's text, so a reader pairs the two by identity, not by parsing.

use serde::{Deserialize, Serialize};

use crate::record::Recorder;
use crate::{Activity, Branch, Error};

/// Where a [`Feedback`] came from. Serialized in snake case.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackSource {
    /// `by pr --watch`: a pull request's reviews, comments and checks.
    PullRequest,
    /// `by review`: comments written on the branch's diff.
    Review,
}

/// What one [`FeedbackItem`] is. Serialized in snake case.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackKind {
    /// A person's comment or review: a correction.
    #[default]
    Comment,
    /// A failed check, CI's or the branch's own: not a correction.
    Check,
}

/// One piece of feedback.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackItem {
    /// A comment to address, or a failed check.
    #[serde(default)]
    pub kind: FeedbackKind,
    /// The file it is about, relative to the repository's root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The (last) line it is about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// The first line of a range, when it covers more than one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    /// Who wrote it, such as a GitHub login.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Its text, as written: not quoted, escaped or numbered.
    pub body: String,
}

/// Feedback sent into a branch, as data.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Feedback {
    /// Where it came from: a pull request or a review.
    pub source: FeedbackSource,
    /// What it is about, such as the pull request's URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// The comments and checks, in the order they were sent.
    pub items: Vec<FeedbackItem>,
    /// The BLAKE3 digest (hex) of the prompt's text, trimmed, that carried
    /// it to the harness, as a turn's prompt or a steer. Set when recorded
    /// by [`Branch::record_feedback`]; a value given is replaced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

/// The digest [`Feedback::prompt`] holds for `prompt`.
pub fn prompt_digest(prompt: &str) -> String {
    blake3::hash(prompt.trim().as_bytes()).to_hex().to_string()
}

impl Branch {
    /// Record `feedback` on this branch's event log as carried by
    /// `prompt`, before the prompt is sent or steered, so a reader of the
    /// log finds the structured feedback beside the prose. Outside any
    /// turn.
    pub fn record_feedback(&self, prompt: &str, mut feedback: Feedback) -> Result<(), Error> {
        feedback.prompt = Some(prompt_digest(prompt));
        let store = self.yard().store();
        Recorder::open(&store, &self.info().name, None)?
            .record(Activity::Feedback(Box::new(feedback)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The digest ignores surrounding whitespace, and an old record without
    /// the optional fields still reads.
    #[test]
    fn digest_is_of_the_trimmed_prompt_and_old_records_read() {
        assert_eq!(prompt_digest("go\n"), prompt_digest("  go"));
        assert_ne!(prompt_digest("go"), prompt_digest("stop"));
        let read: Feedback =
            serde_json::from_str(r#"{"source": "review", "items": [{"body": "x"}]}"#)
                .expect("reads");
        assert_eq!(read.items[0].kind, FeedbackKind::Comment);
        assert_eq!(read.prompt, None);
        let text = serde_json::to_string(&read).expect("writes");
        assert_eq!(
            text,
            r#"{"source":"review","items":[{"kind":"comment","body":"x"}]}"#
        );
    }
}
