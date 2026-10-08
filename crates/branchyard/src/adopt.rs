//! Adopting a harness session that already exists on this machine (one a
//! person started in Claude Code or Codex, outside Branchyard) as a
//! branch: a worktree at the commit it ran on, or at a base with a diff
//! applied, and the session recorded as the branch's own, so the branch's
//! next turn resumes it natively. Finding and reading the sessions is the
//! CLI's (`by adopt`); this only creates the branch. See `docs/usage.md`.

use serde::{Deserialize, Serialize};

use crate::record::Recorder;
use crate::run::{self, NewBranch};
use crate::state::Taken;
use crate::{harness, names, Activity, Branch, BranchStatus, Error, Yard};

/// What `by adopt` recorded about the session it adopted, as the branch's
/// first event.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Adoption {
    /// The harness whose session it is: `claude-code` or `codex`.
    pub harness: String,
    /// The session's own ID, which the branch's next turn resumes.
    pub session: String,
    /// The session's transcript, as read.
    pub source: String,
    /// The directory the session ran in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The commit the worktree was created at.
    pub base: String,
    /// How the base was chosen: `session_commit` (the commit the session
    /// recorded), `head` (this checkout's HEAD), or `head_with_diff` (HEAD,
    /// then the uncommitted diff of the session's directory applied).
    pub how: String,
    /// Files the applied diff changed, when one was.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diff_files: Vec<String>,
    /// What a person should know: what was not carried over, and why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// A branch to create from an existing session.
#[derive(Clone, Debug)]
pub struct AdoptSpec {
    /// The branch's name; one is made from `prompt` when unset.
    pub name: Option<String>,
    /// The branch's task: the session's title or first prompt.
    pub prompt: String,
    /// The harness ID (`claude-code`, `codex`) or a profile ID.
    pub harness: String,
    /// The worktree's commit.
    pub base: String,
    /// A binary diff (`git diff --binary`) applied to the new worktree,
    /// uncommitted, before anything else.
    pub diff: Option<Vec<u8>>,
    /// Recorded as the branch's first event; `base` and the session are
    /// set from it.
    pub adoption: Adoption,
}

/// Create the branch `spec` describes, settled `no_changes` with its
/// session set, so its next turn (`by send`) resumes the session. A diff
/// that does not apply removes the new branch again and fails.
pub(crate) fn adopt(yard: &Yard, spec: AdoptSpec) -> Result<Branch, Error> {
    let profile = harness::select(Some(&spec.harness))?;
    let base = run::resolve_base(yard, Some(&spec.base))?;
    let store = yard.store();
    let name =
        names::reserve(&store, &yard.root, spec.name.as_deref(), &spec.prompt, &[])?.remove(0);
    let record = run::new_record(
        &store,
        NewBranch {
            name: &name,
            prompt: &spec.prompt,
            profile,
            base,
            parent: None,
            check: None,
            command: None,
            home: None,
            cost_baseline: None,
            provider: None,
            grant: None,
            depth: 0,
            deny: Vec::new(),
            provision: None,
            workspace: None,
            seed: None,
            actor: None,
            task: None,
        },
    )
    .inspect_err(|_| store.release(&name))?;
    let lease = match store
        .acquire(&record)
        .inspect_err(|_| store.release(&name))?
    {
        Taken::Granted(lease) => lease,
        Taken::Stale => return Err(Error::Running(name)),
    };
    let (mut record, lease) = run::materialize(yard, record, lease)?;
    if let BranchStatus::Failed { reason } = &record.info.status {
        let reason = reason.clone();
        branchyard_support::best_effort("finish the lease", lease.finish(Some(&record), None));
        return Err(Error::State(format!("{name}: {reason}")));
    }
    if let Some(diff) = spec.diff.as_ref().filter(|d| !d.is_empty()) {
        let applied = branchyard_workspace::Git::new(&record.info.worktree)
            .args(["apply", "--binary", "--whitespace=nowarn", "-"])
            .stdin(diff.clone())
            .run();
        if let Err(error) = applied {
            record.info.status = BranchStatus::Failed {
                reason: format!("the session's diff did not apply: {error}"),
            };
            branchyard_support::best_effort("finish the lease", lease.finish(Some(&record), None));
            branchyard_support::best_effort("remove the branch", yard.remove(&name));
            return Err(Error::State(format!(
                "the session's uncommitted diff does not apply to {}: {error}",
                record.info.base
            )));
        }
    }
    let mut adoption = spec.adoption;
    adoption.base = record.info.base.clone();
    record.info.session = Some(adoption.session.clone());
    record.info.status = BranchStatus::NoChanges;
    let mut recorder = Recorder::fenced(&store, lease.fence(), None);
    recorder.record(Activity::Adopted(Box::new(adoption)))?;
    recorder.finish(lease, &record)?;
    yard.branch(&name)
}
