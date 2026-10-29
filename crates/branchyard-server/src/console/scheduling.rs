//! Native harness routing through the existing worker inventory and queue.

use super::{get, runtime, sdk, text, Collection, ConsoleWork};
use crate::api::{App, Caller};
use crate::error::ApiError;
use crate::ops::Registry;

/// Apply existing native inventory routing to starts and continuations.
pub(super) fn requirements(
    app: &App,
    caller: &Caller,
    repo: &str,
    work: &ConsoleWork,
) -> Result<Vec<String>, ApiError> {
    let (mut labels, named, command, provider) = match work {
        ConsoleWork::Runtime { owner_host, .. } | ConsoleWork::PullRequest { owner_host, .. } => {
            return Ok(vec![format!("worker-host:{owner_host}")]);
        }
        ConsoleWork::Start { request, .. } => {
            let named = if request.harnesses.is_empty() {
                request.harness.iter().cloned().collect::<Vec<_>>()
            } else {
                request.harnesses.clone()
            };
            let targets = named
                .iter()
                .map(|name| Some(name.as_str()))
                .collect::<Vec<_>>();
            let command = app.command(request.command.clone(), &targets)?;
            (
                crate::api::required_labels(&request.require_labels)?,
                named,
                command,
                request.provider.clone(),
            )
        }
        ConsoleWork::Send {
            workspace_id,
            session_id,
            request,
        } => {
            let session = runtime::session(app, caller, workspace_id, session_id)?;
            let harness = text(&session.body, "harness")?;
            let workspace = get(app, Collection::Workspaces, workspace_id)?;
            let state = runtime::resolve(app, &workspace)?;
            let branch = state
                .yard
                .branch(text(&session.body, "branch")?)
                .map_err(sdk)?;
            let command = request
                .command
                .clone()
                .or_else(|| app.config.harness_commands.get(harness).cloned())
                .or(branch.command().map_err(sdk)?);
            (
                crate::api::required_labels(&request.require_labels)?,
                vec![harness.to_owned()],
                command,
                branch.provider().map_err(sdk)?,
            )
        }
        // Controls must remain claimable when a harness is absent or unhealthy.
        _ => return Ok(Vec::new()),
    };
    labels.extend(named_requirements(
        &app.registry,
        repo,
        &named,
        command.as_deref(),
        provider.as_ref(),
    ));
    labels.sort();
    labels.dedup();
    Ok(labels)
}

fn named_requirements(
    registry: &Registry,
    repo: &str,
    named: &[String],
    command: Option<&[String]>,
    provider: Option<&branchyard::Provider>,
) -> Vec<String> {
    if command.is_some() || !branchyard::Provider::is_local(provider) {
        return Vec::new();
    }
    let ids = named
        .iter()
        .filter_map(|name| branchyard::inventory::harness_of(name))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if ids.is_empty() || ids.len() != named.len() {
        return Vec::new();
    }
    registry.harness_requirement(repo, &ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::Options;
    use crate::store::{OperationStore, SqliteStore, Worker};

    #[test]
    fn advertised_native_harness_routes_without_restricting_configured_commands() {
        let store = SqliteStore::memory();
        let worker = Worker::current();
        store
            .beat(
                &worker,
                &["harness:codex".into()],
                &["managed".into()],
                Some(&crate::store::test_inventory("codex")),
            )
            .unwrap();
        let registry =
            Registry::open(Box::new(store), Options::new(vec!["managed".into()])).unwrap();
        assert_eq!(
            named_requirements(&registry, "managed", &["codex".into()], None, None),
            vec!["harness:codex"]
        );
        assert!(named_requirements(
            &registry,
            "managed",
            &["codex".into()],
            Some(&["operator-fixed-command".into()]),
            None
        )
        .is_empty());
        assert!(
            named_requirements(&registry, "other-repo", &["codex".into()], None, None).is_empty()
        );
        let remote = branchyard::Provider::substrate(branchyard::SubstrateOptions::default());
        assert!(
            named_requirements(&registry, "managed", &["codex".into()], None, Some(&remote))
                .is_empty()
        );
        assert!(named_requirements(
            &registry,
            "managed",
            &["unknown-profile".into()],
            None,
            None
        )
        .is_empty());
    }
}
