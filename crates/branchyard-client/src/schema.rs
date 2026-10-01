//! Builds `schema/contract.json`: the JSON Schema for every request and
//! response type of the server's HTTP API (`crate::api` and
//! `crate::storage_api`), generated from their Rust definitions with
//! `schemars` rather than hand-maintained.
//!
//! Only behind the `schema` feature: this pulls `schemars::JsonSchema`
//! derives onto the wire types (and the branch/delegation/provisioning
//! types they reference in `branchyard` and its dependency crates) that a
//! normal build does not need. See `tools/schema-gen` (the generator
//! binary this module backs) and `docs/server.md#published-schema`.
//!
//! Freshness is checked by `tests/contract.rs`, which regenerates this
//! document and compares it with the checked-in `schema/contract.json`;
//! run it with `cargo test -p branchyard-client --features schema --test
//! contract`.

use schemars::{schema_for, Schema};
use serde_json::{Map, Value};

use crate::api;
use crate::storage_api;

/// One schema, generated with its own [`schemars::SchemaGenerator`], keyed
/// by its root type's name. Each entry's `$defs` (nested types it
/// reaches) are folded into the entry's own object so every root is a
/// self-contained schema document.
fn entry<T: schemars::JsonSchema>(name: &str, out: &mut Map<String, Value>) {
    let schema: Schema = schema_for!(T);
    out.insert(
        name.to_owned(),
        Value::Object(schema.to_value().as_object().cloned().unwrap_or_default()),
    );
}

/// The full contract: `{"version": 1, "types": {"TaskRequest": {...}, ...}}`.
/// `version` changes only when this document's own shape changes, not when
/// a type is added; see `docs/server.md#published-schema`.
pub fn contract() -> Value {
    let mut types = Map::new();
    entry::<api::BudgetSpec>("BudgetSpec", &mut types);
    entry::<api::PolicyMode>("PolicyMode", &mut types);
    entry::<api::RuleSpec>("RuleSpec", &mut types);
    entry::<api::PolicySpec>("PolicySpec", &mut types);
    entry::<api::TaskRequest>("TaskRequest", &mut types);
    entry::<api::SendRequest>("SendRequest", &mut types);
    entry::<api::ForkRequest>("ForkRequest", &mut types);
    entry::<api::ReincarnateRequest>("ReincarnateRequest", &mut types);
    entry::<api::MergeRequest>("MergeRequest", &mut types);
    entry::<api::SpawnRequest>("SpawnRequest", &mut types);
    entry::<api::GraphRequest>("GraphRequest", &mut types);
    entry::<branchyard::Graph>("Graph", &mut types);
    entry::<branchyard::GraphApplied>("GraphApplied", &mut types);
    entry::<api::IntegrateRequest>("IntegrateRequest", &mut types);
    entry::<api::SteerRequest>("SteerRequest", &mut types);
    entry::<api::CancelRequest>("CancelRequest", &mut types);
    entry::<api::CancelResult>("CancelResult", &mut types);
    entry::<api::AskRequest>("AskRequest", &mut types);
    entry::<api::TextRequest>("TextRequest", &mut types);
    entry::<api::AnswerRequest>("AnswerRequest", &mut types);
    entry::<api::OperationKind>("OperationKind", &mut types);
    entry::<api::OperationState>("OperationState", &mut types);
    entry::<api::OperationResult>("OperationResult", &mut types);
    entry::<api::Operation>("Operation", &mut types);
    entry::<api::ErrorBody>("ErrorBody", &mut types);
    entry::<api::ErrorResponse>("ErrorResponse", &mut types);
    entry::<api::FeedEntry>("FeedEntry", &mut types);
    entry::<api::BranchEvents>("BranchEvents", &mut types);
    entry::<api::BranchList>("BranchList", &mut types);
    entry::<api::OperationList>("OperationList", &mut types);
    entry::<api::Diff>("Diff", &mut types);
    entry::<api::Removed>("Removed", &mut types);
    entry::<api::RepoEntry>("RepoEntry", &mut types);
    entry::<api::RepoList>("RepoList", &mut types);
    entry::<api::HarnessEntry>("HarnessEntry", &mut types);
    entry::<api::HarnessList>("HarnessList", &mut types);
    entry::<api::MergedInfo>("MergedInfo", &mut types);
    entry::<branchyard::BranchInfo>("BranchInfo", &mut types);
    entry::<branchyard::Inspection>("Inspection", &mut types);
    entry::<branchyard::Children>("Children", &mut types);
    entry::<branchyard::EventPage>("EventPage", &mut types);
    entry::<branchyard::Inbox>("Inbox", &mut types);
    entry::<branchyard::Message>("Message", &mut types);
    entry::<branchyard::Asked>("Asked", &mut types);
    entry::<branchyard::Steer>("Steer", &mut types);
    entry::<branchyard::ArtifactRef>("ArtifactRef", &mut types);
    entry::<branchyard::ScratchArea>("ScratchArea", &mut types);
    entry::<branchyard::ScratchLock>("ScratchLock", &mut types);
    entry::<storage_api::ArtifactList>("ArtifactList", &mut types);
    entry::<storage_api::ShareRequest>("ShareRequest", &mut types);
    entry::<storage_api::CreateScratchRequest>("CreateScratchRequest", &mut types);
    entry::<storage_api::ScratchList>("ScratchList", &mut types);
    entry::<storage_api::LockState>("LockState", &mut types);
    entry::<storage_api::Ack>("Ack", &mut types);
    entry::<storage_api::Empty>("Empty", &mut types);
    Value::Object(Map::from_iter([
        (
            "schema".to_owned(),
            Value::String("branchyard/contract/v1".into()),
        ),
        ("version".to_owned(), Value::Number(1.into())),
        ("types".to_owned(), Value::Object(types)),
    ]))
}

/// [`contract`], pretty-printed with a trailing newline, as
/// `schema/contract.json` is checked in.
pub fn contract_json() -> String {
    format!(
        "{}\n",
        serde_json::to_string_pretty(&contract()).expect("contract serializes")
    )
}
