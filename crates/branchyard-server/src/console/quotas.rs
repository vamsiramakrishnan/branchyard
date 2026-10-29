//! Tenant quota accounting for registered and managed native task yards.

use std::collections::{BTreeSet, HashSet};

use axum::http::StatusCode;
use serde_json::{json, Value};

use crate::api::{App, Caller};
use crate::config::TenantPolicy;
use crate::console_store::{Collection, Mutation};
use crate::error::ApiError;
use crate::store::AdmissionQuota;

use super::{insert, new, sdk, update, MANAGED_REPO};

/// Reserve one bounded short-control admission above a tenant's turn ceiling.
pub(super) fn control_admission(policy: &TenantPolicy) -> AdmissionQuota {
    AdmissionQuota {
        max_running: policy.max_running.map(|max| max.saturating_add(1)),
        max_branches: None,
        existing_branches: None,
    }
}

/// Build native branch ceilings and reserve the tenant snapshot revision before
/// reading yards. The CAS and queue admission commit in the same transaction.
pub(super) fn session_admission(
    app: &App,
    caller: &Caller,
    mutations: &mut Vec<Mutation>,
) -> Result<AdmissionQuota, ApiError> {
    let policy = app.tenant_policy(caller);
    let key = format!("quota:{}", caller.tenant());
    // Read this before the snapshot: any intervening console start changes it,
    // including a turn that finishes before this request enters SQL admission.
    let counter = app.registry.console_get(Collection::Idempotency, &key)?;
    let mut managed_branches = BTreeSet::new();
    let needs_usage = policy.max_cost_usd.is_some() || policy.max_artifact_bytes.is_some();
    let needs_yards = policy.max_branches.is_some() || needs_usage;
    let mut yards: Vec<branchyard::Yard> = if needs_usage {
        app.tenant_repos(caller.tenant())
            .map(|repo| repo.yard.clone())
            .collect()
    } else {
        Vec::new()
    };
    if needs_yards {
        for workspace in app
            .registry
            .console_list(Collection::Workspaces, caller.tenant())?
        {
            if workspace.body.get("repo").and_then(Value::as_str) != Some(MANAGED_REPO) {
                continue;
            }
            let Some(repo) = super::runtime::resolve_if_initialized(app, &workspace)? else {
                continue;
            };
            if policy.max_branches.is_some() {
                managed_branches.extend(
                    repo.yard
                        .branches()
                        .map_err(sdk)?
                        .into_iter()
                        .map(|branch| (MANAGED_REPO.to_owned(), branch.name)),
                );
            }
            if needs_usage {
                yards.push(repo.yard);
            }
        }
    }
    if needs_usage {
        let mut cost = 0.0;
        let mut bytes = 0u64;
        let mut digests = HashSet::new();
        for yard in yards {
            for branch in yard.branches().map_err(sdk)? {
                cost += branch.cost_usd.unwrap_or(0.0);
                for artifact in yard.artifacts(&branch.name).map_err(sdk)? {
                    if digests.insert(artifact.digest) {
                        bytes = bytes.checked_add(artifact.size).ok_or_else(|| {
                            ApiError::internal("tenant artifact byte count overflow")
                        })?;
                    }
                }
            }
        }
        enforce_usage(caller.tenant(), &policy, cost, bytes)?;
    }
    let mut quota = app.admission_quota(caller, &policy);
    if policy.max_branches.is_some() {
        let registered = quota.existing_branches.take();
        quota.existing_branches = Some(Box::new(move || {
            let mut branches = managed_branches.clone();
            if let Some(registered) = &registered {
                branches.extend(registered()?);
            }
            Ok(branches)
        }));
    }
    mutations.push(match counter {
        Some(counter) => {
            if counter.tenant != caller.tenant() {
                return Err(ApiError::internal("workspace quota tenant mismatch"));
            }
            let body = counter.body.clone();
            update(counter, body)
        }
        None => {
            let workspaces = app
                .registry
                .console_list(Collection::Workspaces, caller.tenant())?
                .len();
            insert(new(
                Collection::Idempotency,
                &key,
                caller,
                json!({"workspaces":workspaces}),
            ))
        }
    });
    Ok(quota)
}

fn enforce_usage(
    tenant: &str,
    policy: &TenantPolicy,
    cost: f64,
    bytes: u64,
) -> Result<(), ApiError> {
    let exceeded = if policy.max_cost_usd.is_some_and(|max| cost >= max) {
        Some(("max_cost_usd", json!(policy.max_cost_usd), json!(cost)))
    } else if policy.max_artifact_bytes.is_some_and(|max| bytes >= max) {
        Some((
            "max_artifact_bytes",
            json!(policy.max_artifact_bytes),
            json!(bytes),
        ))
    } else {
        None
    };
    if let Some((limit, max, held)) = exceeded {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "quota_exceeded",
            format!("tenant {tenant} is at its {limit} quota"),
        )
        .detail(json!({"tenant":tenant,"limit":limit,"max":max,"reserved":held})));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_admission_allows_one_slot_at_saturation_and_refuses_another() {
        use crate::store::{Admission, OperationStore, SqliteStore, StoredOperation};
        let policy = TenantPolicy {
            max_running: Some(1),
            max_branches: Some(0),
            ..Default::default()
        };
        let quota = control_admission(&policy);
        assert_eq!(quota.max_running, Some(2));
        assert!(quota.max_branches.is_none());
        assert!(quota.existing_branches.is_none());
        let store = SqliteStore::memory();
        let operation = |id: &str| -> StoredOperation {
            serde_json::from_value(json!({
            "operation":{"id":id,"repo":"managed","kind":"task","state":"queued","branches":[],"cursor":0,"created_at_ms":0},
            "tenant":"tenant"
        })).unwrap()
        };
        assert_eq!(
            store
                .admit(
                    &operation("normal"),
                    &json!({"kind":"task"}),
                    &AdmissionQuota::default()
                )
                .unwrap(),
            Admission::Admitted
        );
        let work = json!({"kind":"console","command":{"action":"control","control":"cancel","workspace_id":"w"}});
        assert_eq!(
            store
                .admit(&operation("first-control"), &work, &quota)
                .unwrap(),
            Admission::Admitted
        );
        assert_eq!(
            store
                .admit(&operation("second-control"), &work, &quota)
                .unwrap(),
            Admission::Quota {
                limit: "max_running",
                max: 2,
                reserved: 2
            }
        );
        assert!(store.get("second-control").unwrap().is_none());
        assert_eq!(store.queue().unwrap().len(), 2);
        assert_eq!(
            control_admission(&TenantPolicy {
                max_running: Some(usize::MAX),
                ..Default::default()
            })
            .max_running,
            Some(usize::MAX)
        );
    }

    #[test]
    fn usage_limits_match_registered_repository_semantics() {
        let policy = TenantPolicy {
            max_cost_usd: Some(2.0),
            max_artifact_bytes: Some(1024),
            ..Default::default()
        };
        assert!(enforce_usage("tenant", &policy, 1.99, 1023).is_ok());
        let cost = enforce_usage("tenant", &policy, 2.0, 0).unwrap_err();
        assert_eq!(cost.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(cost.body.detail.as_ref().unwrap()["limit"], "max_cost_usd");
        let bytes = enforce_usage("tenant", &policy, 0.0, 1024).unwrap_err();
        assert_eq!(
            bytes.body.detail.as_ref().unwrap()["limit"],
            "max_artifact_bytes"
        );
    }
}
