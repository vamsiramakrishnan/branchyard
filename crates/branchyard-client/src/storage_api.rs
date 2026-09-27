//! Wire types for artifacts and scratch areas over HTTP; see
//! `docs/storage.md`. Kept in its own module (client methods live in a
//! clearly separated section of `lib.rs`, and the server's handlers in its
//! own `storage_routes` module) so this feature's routes and client
//! methods are easy to merge alongside unrelated work on the same crates.

use branchyard::{ArtifactRef, ScratchArea, ScratchLock};
use serde::{Deserialize, Serialize};

/// `GET .../artifacts`: every artifact the acting branch may read.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ArtifactList {
    pub artifacts: Vec<ArtifactRef>,
}

/// `POST .../artifacts/{id}/share` and `.../scratch/{name}/share`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShareRequest {
    pub to: String,
}

/// `POST .../scratch`: create a scratch area owned by the acting branch.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateScratchRequest {
    pub name: String,
}

/// `GET .../scratch`: every scratch area the acting branch may reach.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ScratchList {
    pub areas: Vec<ScratchArea>,
}

/// `GET .../scratch/{name}/lock`: the area's writer lock, if any is held.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LockState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock: Option<ScratchLock>,
}

/// A trivial success, printed as `{"ok": true}`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ack {
    pub ok: bool,
}

/// `POST .../scratch/{name}/lock` and `.../unlock`: no fields yet.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

/// The header a downloaded artifact's content carries its digest under,
/// checked by the client against the metadata it already has.
pub const DIGEST_HEADER: &str = "x-branchyard-artifact-digest";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_reject_unknown_fields() {
        let error =
            serde_json::from_value::<ShareRequest>(serde_json::json!({"to": "b", "toward": "c"}))
                .unwrap_err();
        assert!(error.to_string().contains("toward"), "{error}");
    }
}
