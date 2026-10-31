//! What the console adds to a person's text in each turn's harness input,
//! and how a recorded prompt gives that text back for display.

use super::Record;
use branchyard::ResultContract;

const REVIEW_PREFACE: &str =
    "Review the frozen candidate and input artifacts. Do not modify or integrate writer branches.\n";

/// What the server puts around a person's text in one turn's harness input,
/// in the order the harness reads it: the preface, the person's text, the
/// input hint, then the requested result. A part a turn lacks is empty.
pub(super) struct Additions {
    preface: &'static str,
    hint: String,
    requested: String,
}

impl Additions {
    /// The first turn of `session`: a reviewer's preface, the sentence that
    /// links selected inputs' index, and the requested result of `contract`.
    pub(super) fn first(
        workspace_id: &str,
        session: &Record,
        contract: Option<&ResultContract>,
    ) -> Self {
        Self {
            preface: if session.body["access"] == "reviewer" {
                REVIEW_PREFACE
            } else {
                ""
            },
            hint: super::sources::index(workspace_id, session)
                .map(|index| super::sources::input_hint(&index))
                .unwrap_or_default(),
            requested: branchyard::contract::prompt("", contract),
        }
    }

    /// A later turn: only the requested result of `contract`. The engine
    /// appends it to a first turn alone, and every turn of a record with a
    /// capture contract must rewrite the manifest.
    pub(super) fn later(contract: Option<&ResultContract>) -> Self {
        Self {
            preface: "",
            hint: String::new(),
            requested: branchyard::contract::prompt("", contract),
        }
    }

    /// The harness input for a person's `text`.
    pub(super) fn compose(&self, text: &str) -> String {
        format!("{}{text}{}{}", self.preface, self.hint, self.requested)
    }

    /// The person's text in a recorded `prompt`: each addition is removed
    /// only where it stands exactly, so nothing a person wrote changes.
    pub(super) fn typed<'a>(&self, prompt: &'a str) -> &'a str {
        let prompt = prompt
            .strip_suffix(self.requested.as_str())
            .unwrap_or(prompt);
        let prompt = prompt.strip_suffix(self.hint.as_str()).unwrap_or(prompt);
        prompt.strip_prefix(self.preface).unwrap_or(prompt)
    }
}

#[cfg(test)]
mod tests {
    use super::Additions;
    use crate::console_store::{Collection, Record};
    use serde_json::{json, Value};

    fn session(body: Value) -> Record {
        Record {
            collection: Collection::Sessions,
            id: "s-1".into(),
            tenant: "tenant".into(),
            revision: 1,
            body,
        }
    }

    fn contract() -> branchyard::ResultContract {
        serde_json::from_value(json!({"schema":{"type":"object","required":["summary"],"properties":{"summary":{"type":"string"}}},
            "required":true,"purpose":"deliverable","capture":{"manifest":"outputs/artifacts.json"}}))
        .unwrap()
    }

    #[test]
    fn every_composed_turn_gives_back_exactly_the_typed_text() {
        let contract = contract();
        let writer = session(json!({"access":"writer","sources":["source"]}));
        let reviewer = session(json!({"access":"reviewer","artifacts":[{"id":"a"}]}));
        let bare = session(json!({"access":"writer","sources":[]}));
        let text = "Summarise the figures.\n\nKeep it short.";
        for additions in [
            Additions::first("w", &writer, Some(&contract)),
            Additions::first("w", &reviewer, None),
            Additions::first("w", &bare, Some(&contract)),
            Additions::later(Some(&contract)),
            Additions::later(None),
        ] {
            assert_eq!(additions.typed(&additions.compose(text)), text);
        }
        let composed = Additions::first("w", &writer, Some(&contract)).compose(text);
        assert!(composed.contains(".console-inputs/w/s-1/INDEX.md"));
        assert!(composed.contains("write artifact manifest `outputs/artifacts.json`"));
        assert!(Additions::first("w", &reviewer, None)
            .compose(text)
            .starts_with("Review the frozen candidate"));
        assert_eq!(Additions::later(None).compose(text), text);
    }

    #[test]
    fn a_start_the_engine_completes_matches_the_first_turn_the_console_reads_back() {
        let contract = contract();
        let writer = session(json!({"access":"writer","sources":["source"]}));
        let submitted = Additions::first("w", &writer, None).compose("Summarise.");
        assert_eq!(
            branchyard::contract::prompt(&submitted, Some(&contract)),
            Additions::first("w", &writer, Some(&contract)).compose("Summarise.")
        );
    }

    #[test]
    fn text_the_server_did_not_add_is_never_removed() {
        let contract = contract();
        let requested = branchyard::contract::prompt("", Some(&contract));
        let pasted = format!("Use this block:{requested}");
        assert_eq!(Additions::later(None).typed(&pasted), pasted);
        let hint = super::super::sources::input_hint(".console-inputs/w/s-1/INDEX.md");
        let quoted = format!("Earlier you were told:{hint}");
        let bare = session(json!({"access":"writer"}));
        assert_eq!(Additions::first("w", &bare, None).typed(&quoted), quoted);
        let preface = "Review the frozen candidate and input artifacts. Do not modify or integrate writer branches.\nThen this.";
        assert_eq!(Additions::first("w", &bare, None).typed(preface), preface);
        assert_eq!(Additions::later(None).typed(preface), preface);
    }
}
