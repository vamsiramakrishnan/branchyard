//! Builds `schema/contract.json`: the JSON Schema for every request and
//! response type of the server's HTTP API (`crate::api`,
//! `crate::storage_api` and `crate::triggers`), generated from their Rust definitions with
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
use crate::companion;
use crate::effects_api;
use crate::knowledge_api;
use crate::storage_api;
use crate::triggers;

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
    entry::<api::GoalRequest>("GoalRequest", &mut types);
    entry::<api::MapRequest>("MapRequest", &mut types);
    entry::<api::MapResumeRequest>("MapResumeRequest", &mut types);
    entry::<api::MapList>("MapList", &mut types);
    entry::<api::TaskList>("TaskList", &mut types);
    entry::<branchyard::tasks::TaskView>("TaskView", &mut types);
    entry::<api::RegisterServiceRequest>("RegisterServiceRequest", &mut types);
    entry::<api::ServiceList>("ServiceList", &mut types);
    entry::<api::WellKnown>("WellKnown", &mut types);
    entry::<branchyard::MapReport>("MapReport", &mut types);
    entry::<knowledge_api::KnowledgeList>("KnowledgeList", &mut types);
    entry::<knowledge_api::KnowledgeAddRequest>("KnowledgeAddRequest", &mut types);
    entry::<knowledge_api::KnowledgeDecisionRequest>("KnowledgeDecisionRequest", &mut types);
    entry::<knowledge_api::KnowledgeEditRequest>("KnowledgeEditRequest", &mut types);
    entry::<knowledge_api::KnowledgeExport>("KnowledgeExport", &mut types);
    entry::<knowledge_api::DistillRequest>("DistillRequest", &mut types);
    entry::<knowledge_api::PlanApproveRequest>("PlanApproveRequest", &mut types);
    entry::<knowledge_api::PlanRejectRequest>("PlanRejectRequest", &mut types);
    entry::<branchyard::KnowledgeEntry>("KnowledgeEntry", &mut types);
    entry::<effects_api::ApprovalList>("ApprovalList", &mut types);
    entry::<effects_api::ApprovalAnswerRequest>("ApprovalAnswerRequest", &mut types);
    entry::<effects_api::EffectList>("EffectList", &mut types);
    entry::<effects_api::EffectDetail>("EffectDetail", &mut types);
    entry::<effects_api::EffectActionRequest>("EffectActionRequest", &mut types);
    entry::<effects_api::UndoRequest>("UndoRequest", &mut types);
    entry::<effects_api::UndoReport>("UndoReport", &mut types);
    entry::<branchyard::effects::ApprovalAsk>("ApprovalAsk", &mut types);
    entry::<branchyard::effects::EffectEntry>("EffectEntry", &mut types);
    entry::<branchyard::effects::undo::UndoPlan>("UndoPlan", &mut types);
    entry::<branchyard::effects::reconcile::Reconciled>("Reconciled", &mut types);
    entry::<branchyard::Distilled>("Distilled", &mut types);
    entry::<branchyard::PlanInfo>("PlanInfo", &mut types);
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
    entry::<api::WorkerInventory>("WorkerInventory", &mut types);
    entry::<api::InventoryReport>("InventoryReport", &mut types);
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
    entry::<triggers::When>("When", &mut types);
    entry::<triggers::EventSource>("EventSource", &mut types);
    entry::<triggers::Conditions>("Conditions", &mut types);
    entry::<triggers::Precheck>("Precheck", &mut types);
    entry::<triggers::RouteSpec>("RouteSpec", &mut types);
    entry::<triggers::TriggerPolicy>("TriggerPolicy", &mut types);
    entry::<triggers::TriggerSpec>("TriggerSpec", &mut types);
    entry::<triggers::Trigger>("Trigger", &mut types);
    entry::<triggers::TriggerCreated>("TriggerCreated", &mut types);
    entry::<triggers::TriggerList>("TriggerList", &mut types);
    entry::<triggers::SecretRequest>("SecretRequest", &mut types);
    entry::<triggers::SecretSet>("SecretSet", &mut types);
    entry::<triggers::TriggerTestRequest>("TriggerTestRequest", &mut types);
    entry::<triggers::TriggerEvent>("TriggerEvent", &mut types);
    entry::<triggers::PrecheckResult>("PrecheckResult", &mut types);
    entry::<triggers::TriggerTest>("TriggerTest", &mut types);
    entry::<triggers::RunState>("RunState", &mut types);
    entry::<triggers::RunOutcome>("RunOutcome", &mut types);
    entry::<triggers::TriggerRun>("TriggerRun", &mut types);
    entry::<triggers::TriggerRuns>("TriggerRuns", &mut types);
    entry::<triggers::FireAck>("FireAck", &mut types);
    entry::<triggers::TriggerRemoved>("TriggerRemoved", &mut types);
    entry::<triggers::TriggerToggle>("TriggerToggle", &mut types);
    entry::<companion::PairRequest>("PairRequest", &mut types);
    entry::<companion::Paired>("Paired", &mut types);
    entry::<companion::Me>("Me", &mut types);
    entry::<companion::PushInfo>("PushInfo", &mut types);
    entry::<companion::PushSubscribe>("PushSubscribe", &mut types);
    entry::<companion::PushUnsubscribe>("PushUnsubscribe", &mut types);
    entry::<companion::PushResult>("PushResult", &mut types);
    entry::<companion::PushTest>("PushTest", &mut types);
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
#[allow(clippy::expect_used)] // ratchet: branchyard-client
pub fn contract_json() -> String {
    format!(
        "{}\n",
        serde_json::to_string_pretty(&contract()).expect("contract serializes")
    )
}
