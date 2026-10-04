//! A repository's reaper: what a [`Reclaim`] in its registry names is
//! reclaimed through the code paths that already own it. A process is
//! stopped as recovery stops a harness's (its pid and start time checked);
//! a sandbox or recipe machine goes through the branch's recovery first,
//! which brings its work back and destroys what the turn journaled, and is
//! then destroyed through its provider only if it is still there and not
//! kept for the branch's next turn; pool slots are reclaimed as recovery
//! reclaims them.

use std::path::Path;

use super::{reap, reclaim_process, Outcome, Reaped, Reclaim, Service};
use crate::state::SandboxKind;
use crate::{Error, Provider, Yard};

/// Expire and reclaim what the yard's registry holds.
pub(crate) fn sweep(yard: &Yard, now_ms: u64) -> Result<Vec<Reaped>, Error> {
    let registry = yard.services()?;
    let reaper = |service: &Service, reclaim: &Reclaim| reclaim_one(yard, service, reclaim);
    Ok(reap(&*registry, now_ms, &reaper)?)
}

fn reclaim_one(yard: &Yard, _service: &Service, reclaim: &Reclaim) -> Outcome {
    match reclaim {
        Reclaim::Process { .. } => reclaim_process(reclaim),
        Reclaim::Sandbox {
            root,
            branch,
            provider,
            sandbox,
        } => match other_yard(yard, root) {
            Ok(owner) => sandbox_outcome(&owner, branch, provider, sandbox),
            Err(why) => Outcome::Failed(why),
        },
        Reclaim::PoolSlots { root } => match other_yard(yard, root) {
            Ok(owner) => {
                let removed = crate::pool::reclaim(&owner);
                Outcome::Done(match removed.is_empty() {
                    true => "no pool slot was left".into(),
                    false => format!("removed {} pool slot(s)", removed.len()),
                })
            }
            Err(why) => Outcome::Failed(why),
        },
    }
}

/// `yard` itself when `root` is its root, else the repository at `root`.
fn other_yard(yard: &Yard, root: &Path) -> Result<Yard, String> {
    if root == yard.root() {
        return Ok(yard.clone());
    }
    Yard::open(root).map_err(|e| format!("could not open {}: {e}", root.display()))
}

fn sandbox_outcome(yard: &Yard, branch: &str, provider: &Provider, sandbox: &str) -> Outcome {
    // The branch's own recovery first: it brings the harness's work back
    // and destroys the sandbox its turn journaled.
    if let Err(error) = crate::recover::settle(yard, branch) {
        return Outcome::Failed(format!("recovering branch {branch}: {error}"));
    }
    let kept = yard
        .store()
        .sandboxes()
        .sandboxes(branch)
        .map(|rows| {
            rows.iter()
                .any(|row| row.kind == SandboxKind::Kept && row.name == sandbox)
        })
        .unwrap_or(false);
    if kept {
        return Outcome::Done(format!(
            "sandbox {sandbox} is kept paused for branch {branch}'s next turn"
        ));
    }
    match destroy(yard, provider, sandbox) {
        Ok(said) => Outcome::Done(said),
        Err(why) => Outcome::Failed(why),
    }
}

fn destroy(yard: &Yard, provider: &Provider, sandbox: &str) -> Result<String, String> {
    provider.kind().destroy(yard, sandbox)
}
