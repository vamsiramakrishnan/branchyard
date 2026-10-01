//! Wire types and calls for repository knowledge and plan approval: the
//! server's `/knowledge` routes and a branch's `/plan` routes. Kept in its
//! own module, like [`crate::storage_api`], so this feature's additions
//! merge easily alongside unrelated work. See `docs/knowledge.md` and
//! `docs/plans-and-goals.md`.

use branchyard::{Distilled, KnowledgeEntry, KnowledgeScope, KnowledgeStatus, PlanInfo};
use serde::{Deserialize, Serialize};

use crate::api::{Operation, SendRequest};
use crate::http::encode;
use crate::{new_key, Error, Repo};

/// `GET /v1/repos/{repo}/knowledge[?status=...]`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeList {
    pub entries: Vec<KnowledgeEntry>,
}

/// `POST /v1/repos/{repo}/knowledge`: an entry a person wrote, adopted by
/// the caller unless `propose`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeAddRequest {
    pub text: String,
    #[serde(default)]
    pub scope: KnowledgeScope,
    #[serde(default)]
    pub propose: bool,
}

/// `POST .../knowledge/{id}/adopt` and `.../reject`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeDecisionRequest {
    /// Why, for a rejection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `POST .../knowledge/{id}/edit`: each field changes when given; `""`
/// clears `path` or `kind`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeEditRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// `GET /v1/repos/{repo}/knowledge/export`: the adopted entries as an
/// `AGENTS.md`-style file.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeExport {
    pub markdown: String,
    pub entries: Vec<u64>,
}

/// `POST .../branches/{branch}/distill`: the deterministic extractor.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DistillRequest {}

/// `POST .../branches/{branch}/plan/approve`: the plan, or `edited` in its
/// place, run as the branch's next turn with `send`'s limits and policy
/// (its `prompt` is ignored).
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanApproveRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited: Option<String>,
    #[serde(default)]
    pub send: SendRequest,
}

/// `POST .../branches/{branch}/plan/reject`: the branch fails, or with
/// `replan`, plans again with `reason` under `send`'s limits.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanRejectRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default)]
    pub replan: bool,
    #[serde(default)]
    pub send: SendRequest,
}

impl Repo {
    fn knowledge_path(&self, rest: &str) -> String {
        format!("/v1/repos/{}/knowledge{rest}", encode(self.name()))
    }

    fn plan_path(&self, branch: &str, rest: &str) -> String {
        format!(
            "/v1/repos/{}/branches/{}/plan{rest}",
            encode(self.name()),
            encode(branch)
        )
    }

    /// The repository's knowledge entries, or those of `status`; like `by
    /// knowledge list`.
    pub fn knowledge(&self, status: Option<KnowledgeStatus>) -> Result<Vec<KnowledgeEntry>, Error> {
        let path = match status {
            Some(status) => self.knowledge_path(&format!("?status={}", status.as_str())),
            None => self.knowledge_path(""),
        };
        Ok(self.client().get::<KnowledgeList>(&path)?.entries)
    }

    pub fn knowledge_entry(&self, id: u64) -> Result<KnowledgeEntry, Error> {
        self.client().get(&self.knowledge_path(&format!("/{id}")))
    }

    /// Add an entry as the caller: adopted, unless `propose`.
    pub fn add_knowledge(&self, request: &KnowledgeAddRequest) -> Result<KnowledgeEntry, Error> {
        self.client()
            .post(&self.knowledge_path(""), request, &new_key())
    }

    pub fn adopt_knowledge(&self, id: u64) -> Result<KnowledgeEntry, Error> {
        self.client().post(
            &self.knowledge_path(&format!("/{id}/adopt")),
            &KnowledgeDecisionRequest::default(),
            &new_key(),
        )
    }

    pub fn reject_knowledge(&self, id: u64, reason: Option<&str>) -> Result<KnowledgeEntry, Error> {
        self.client().post(
            &self.knowledge_path(&format!("/{id}/reject")),
            &KnowledgeDecisionRequest {
                reason: reason.map(str::to_owned),
            },
            &new_key(),
        )
    }

    pub fn edit_knowledge(
        &self,
        id: u64,
        request: &KnowledgeEditRequest,
    ) -> Result<KnowledgeEntry, Error> {
        self.client().post(
            &self.knowledge_path(&format!("/{id}/edit")),
            request,
            &new_key(),
        )
    }

    pub fn remove_knowledge(&self, id: u64) -> Result<KnowledgeEntry, Error> {
        self.client()
            .delete(&self.knowledge_path(&format!("/{id}")))
    }

    pub fn export_knowledge(&self) -> Result<KnowledgeExport, Error> {
        self.client().get(&self.knowledge_path("/export"))
    }

    /// Propose entries from `branch` with the deterministic extractor; like
    /// `by knowledge distill`.
    pub fn distill(&self, branch: &str) -> Result<Distilled, Error> {
        self.client().post(
            &format!(
                "/v1/repos/{}/branches/{}/distill",
                encode(self.name()),
                encode(branch)
            ),
            &DistillRequest::default(),
            &new_key(),
        )
    }

    /// `branch`'s plan; like `by plan show`.
    pub fn plan(&self, branch: &str) -> Result<PlanInfo, Error> {
        self.client().get(&self.plan_path(branch, ""))
    }

    /// Approve `branch`'s plan: an operation running its next turn.
    pub fn approve_plan(
        &self,
        branch: &str,
        request: &PlanApproveRequest,
        key: &str,
    ) -> Result<Operation, Error> {
        self.client()
            .post(&self.plan_path(branch, "/approve"), request, key)
    }

    /// Reject `branch`'s plan: an operation that ends it, or runs its next
    /// planning turn.
    pub fn reject_plan(
        &self,
        branch: &str,
        request: &PlanRejectRequest,
        key: &str,
    ) -> Result<Operation, Error> {
        self.client()
            .post(&self.plan_path(branch, "/reject"), request, key)
    }
}
