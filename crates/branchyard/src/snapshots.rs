//! Provider snapshots under git checkpoints: a branch's sandbox kept
//! between turns, provider snapshots taken with its checkpoints, and new
//! sandboxes branched from them for forks, rewinds, delegated children and
//! fans. See `docs/sandbox-snapshots.md`.
//!
//! Git stays the source of truth for the code: a checkpoint is a ref, and a
//! fork's worktree is created from its commit whatever happens here. What a
//! provider snapshot adds is the sandbox around the code: its root disk
//! (toolchains, caches, anything setup installed outside the worktree) and,
//! with a live branch, its memory and processes. Everything here is chosen
//! by the provider's declared [`Capabilities`], never by its name, and every
//! path has a fallback: a fresh sandbox, the worktree from git, and the
//! branch's `[workspace]` setup, with the reason recorded.
//!
//! - **Kept sandbox.** With `keep = "pause"` and a provider that can pause,
//!   a turn's sandbox is paused when the turn ends instead of destroyed, and
//!   recorded in the store ([`SandboxKind::Kept`]); the next turn takes the
//!   row and resumes it. A row whose sandbox is gone falls back to a fresh
//!   one. At most `max_paused` are kept per provider in the store; parking
//!   one more destroys the least recently used.
//! - **Snapshots.** Each checkpoint of a branch with a kept sandbox also
//!   takes a provider snapshot, by capability: with live branching and
//!   pause, the paused sandbox is branched into a paused child named for
//!   the checkpoint (the pattern mario-never-dies probes); otherwise, with
//!   checkpoint and branch, a provider checkpoint (Substrate: suspend, then
//!   a tag). The newest `snapshots` are kept per branch; older ones are
//!   released.
//! - **Seeds.** A branch created from another's checkpoint (a fork, a
//!   delegated child, a rig seat, a graph dependent) or rewound to one of its
//!   own records a [`SandboxSeed`]. Its next turn without a kept sandbox
//!   branches from the matching snapshot, rebinding its own worktree and
//!   home, and inherits the source's workspace setup.

use std::sync::Arc;

use branchyard_sandbox::{
    Capabilities, Checkpoint as ProviderCheckpoint, Consistency, Locality, ProviderError,
    SandboxInfo, SandboxProvider, SandboxSpec, SandboxState, SnapshotGuarantee, SnapshotScope,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::state::{Fence, Record, SandboxKind, SandboxRow, Store};
use crate::{Activity, Provider, Yard};
use branchyard_support::time::now_ms;

/// Checkpoints that keep a provider snapshot, by default.
pub const DEFAULT_SNAPSHOTS: u32 = 3;
/// Kept sandboxes per provider in a store, by default.
pub const DEFAULT_MAX_PAUSED: u32 = 4;

/// The journaled step that parks a turn's sandbox.
pub(crate) const STEP_PARK: &str = "sandbox_park";
/// The journaled step that takes a checkpoint's provider snapshot.
pub(crate) const STEP_SNAPSHOT: &str = "sandbox_snapshot";

/// What happens to a branch's sandbox when a turn ends.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxKeep {
    /// Destroyed: every turn gets a fresh one.
    #[default]
    Destroy,
    /// Paused and recorded, and resumed by the next turn, when the provider
    /// can pause; destroyed otherwise, saying why.
    Pause,
}

impl SandboxKeep {
    pub fn is_destroy(&self) -> bool {
        *self == SandboxKeep::Destroy
    }
}

/// How a provider snapshot was taken, and so how a sandbox is branched
/// from it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotMethod {
    /// A paused child of the branch's sandbox (live branch), branched
    /// again for each new sandbox.
    LiveBranch,
    /// A provider checkpoint (Substrate: suspend and a tag), from which new
    /// sandboxes are created.
    Checkpoint,
}

impl SnapshotMethod {
    /// How it reads in a log: `live branch` or `checkpoint`.
    pub fn describe(self) -> &'static str {
        match self {
            SnapshotMethod::LiveBranch => "live branch",
            SnapshotMethod::Checkpoint => "checkpoint",
        }
    }
}

/// What a snapshot captured: [`SnapshotScope`] as recorded.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxScope {
    /// Memory, processes and disk.
    Full,
    /// The disk only; a restore boots it fresh.
    Disk,
}

/// [`Consistency`] as recorded.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxConsistency {
    /// Taken without the workload's cooperation.
    Crash,
    /// At a point the workload reached deliberately.
    Application,
}

/// The provider snapshot a checkpoint took, on
/// [`crate::Checkpoint::sandbox`].
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxSnapshot {
    /// `microsandbox` or `substrate`.
    pub provider: String,
    /// The provider's name for it: a paused sandbox, or a checkpoint
    /// reference such as a Substrate tag.
    pub handle: String,
    pub scope: SandboxScope,
    pub consistency: SandboxConsistency,
    pub method: SnapshotMethod,
}

/// Where a turn's sandbox came from.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "from", rename_all = "snake_case")]
pub enum SandboxOrigin {
    /// Created fresh; `reason` says why a kept or branched one was not
    /// used, when one was wanted.
    Fresh {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// The branch's own sandbox, kept since its last turn, resumed.
    Resumed,
    /// Branched from `branch`'s provider snapshot at checkpoint `turn`.
    Branched {
        branch: String,
        turn: u32,
        method: SnapshotMethod,
    },
    /// Branched, with its fan's other branches, from one sandbox whose
    /// setup ran once, for `branch`'s worktree.
    Prepared {
        branch: String,
        method: SnapshotMethod,
    },
    /// Branched from the prepared environment of its key (`key`), or, when
    /// that key's build failed, from the last good one (`used`, with
    /// `reason`). See `docs/environments.md`.
    Environment {
        key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        used: Option<String>,
        method: SnapshotMethod,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

/// What happened to a branch's sandbox, as [`Activity::Sandbox`].
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum SandboxEvent {
    /// The turn's sandbox, and where it came from.
    Started {
        provider: String,
        sandbox: String,
        origin: SandboxOrigin,
    },
    /// Paused and recorded when the turn ended, for the next turn.
    Kept { provider: String, sandbox: String },
    /// Destroyed when the turn ended although it was to be kept.
    NotKept {
        provider: String,
        sandbox: String,
        reason: String,
    },
    /// Another branch's kept sandbox, destroyed to stay within
    /// `max_paused`: the least recently used.
    Evicted {
        provider: String,
        sandbox: String,
        branch: String,
    },
    /// A checkpoint's provider snapshot, released to keep the newest
    /// `snapshots`, or with its branch's sandbox.
    Released {
        provider: String,
        handle: String,
        turn: u32,
    },
    /// No provider snapshot was taken with checkpoint `turn`.
    NoSnapshot { turn: u32, reason: String },
}

impl SandboxEvent {
    /// One line for a log, starting with `sandbox:`.
    pub fn describe(&self) -> String {
        match self {
            SandboxEvent::Started {
                provider,
                sandbox,
                origin,
            } => match origin {
                SandboxOrigin::Fresh { reason: None } => {
                    format!("sandbox: fresh ({provider} {sandbox})")
                }
                SandboxOrigin::Fresh {
                    reason: Some(reason),
                } => format!("sandbox: fresh ({reason})"),
                SandboxOrigin::Resumed => {
                    format!("sandbox: resumed the kept {provider} sandbox {sandbox}")
                }
                SandboxOrigin::Branched {
                    branch,
                    turn,
                    method,
                } => format!(
                    "sandbox: branched from {branch}'s checkpoint {turn} ({provider} {})",
                    method.describe()
                ),
                SandboxOrigin::Prepared { branch, method } => format!(
                    "sandbox: branched from the fan's prepared sandbox, set up once in {branch}'s \
                     worktree ({provider} {})",
                    method.describe()
                ),
                SandboxOrigin::Environment {
                    key,
                    used: None,
                    method,
                    ..
                } => format!(
                    "sandbox: branched from prepared environment {} ({provider} {})",
                    &key[..key.len().min(12)],
                    method.describe()
                ),
                SandboxOrigin::Environment {
                    used: Some(used),
                    method,
                    reason,
                    ..
                } => format!(
                    "sandbox: branched from the last good environment {} ({provider} {}): {}",
                    &used[..used.len().min(12)],
                    method.describe(),
                    reason.as_deref().unwrap_or("its own build failed")
                ),
            },
            SandboxEvent::Kept { provider, sandbox } => {
                format!("sandbox: kept paused for the next turn ({provider} {sandbox})")
            }
            SandboxEvent::NotKept { reason, .. } => format!("sandbox: not kept ({reason})"),
            SandboxEvent::Evicted {
                provider,
                sandbox,
                branch,
            } => format!(
                "sandbox: destroyed {branch}'s kept {provider} sandbox {sandbox}, the least \
                 recently used, to stay within max_paused"
            ),
            SandboxEvent::Released {
                provider,
                handle,
                turn,
            } => format!("sandbox: released checkpoint {turn}'s {provider} snapshot {handle}"),
            SandboxEvent::NoSnapshot { turn, reason } => {
                format!("sandbox: no snapshot with checkpoint {turn} ({reason})")
            }
        }
    }
}

/// `event` as an activity to record.
pub(crate) fn event(event: SandboxEvent) -> Activity {
    Activity::Sandbox(Box::new(event))
}

/// Where a branch's next sandbox comes from when it has none kept: the
/// provider snapshot of `branch` at checkpoint `turn`, or the newest at
/// `commit`. Only a snapshot of that commit is used: rows go with their
/// branch, and a recreated branch's checkpoints have their own commits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SandboxSeed {
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
    pub commit: String,
}

/// A snapshot or kept sandbox's provider details, stored as
/// [`SandboxRow::detail`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Detail {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<SnapshotMethod>,
    /// For a checkpoint: the sandbox it was taken from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<SandboxScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consistency: Option<SandboxConsistency>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub portable: Option<bool>,
    /// The checkpoint's commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

impl Detail {
    fn parse(text: &str) -> Detail {
        serde_json::from_str(text).unwrap_or_default()
    }

    fn guarantee(&self) -> SnapshotGuarantee {
        SnapshotGuarantee {
            scope: match self.scope {
                Some(SandboxScope::Disk) => SnapshotScope::Disk,
                _ => SnapshotScope::Full,
            },
            consistency: match self.consistency {
                Some(SandboxConsistency::Application) => Consistency::Application,
                _ => Consistency::Crash,
            },
            locality: match self.portable {
                Some(true) => Locality::Portable,
                _ => Locality::SameHost,
            },
        }
    }

    fn of(guarantee: &SnapshotGuarantee) -> Detail {
        Detail {
            scope: Some(match guarantee.scope {
                SnapshotScope::Full => SandboxScope::Full,
                SnapshotScope::Disk => SandboxScope::Disk,
            }),
            consistency: Some(match guarantee.consistency {
                Consistency::Crash => SandboxConsistency::Crash,
                Consistency::Application => SandboxConsistency::Application,
            }),
            portable: Some(guarantee.locality == Locality::Portable),
            ..Detail::default()
        }
    }

    fn text(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// What a branch's provider options ask of its sandbox's lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Lifecycle {
    pub keep: bool,
    pub snapshots: u32,
    pub max_paused: u32,
}

impl Lifecycle {
    /// What provider options with these settings ask for; `snapshots` and
    /// `max_paused` default when unset.
    pub(crate) fn of(
        keep: SandboxKeep,
        snapshots: Option<u32>,
        max_paused: Option<u32>,
    ) -> Lifecycle {
        Lifecycle {
            keep: keep == SandboxKeep::Pause,
            snapshots: snapshots.unwrap_or(DEFAULT_SNAPSHOTS),
            max_paused: max_paused.unwrap_or(DEFAULT_MAX_PAUSED),
        }
    }
}

/// The lifecycle `provider` asks for; `None` for a local harness.
pub(crate) fn lifecycle(provider: Option<&Provider>) -> Option<Lifecycle> {
    crate::providers::of(provider).lifecycle()
}

/// `local`, `microsandbox`, `substrate` or `recipe`.
pub(crate) fn provider_name(provider: &Provider) -> &'static str {
    provider.kind().name()
}

/// Which provider, and where, holds a row: sandboxes and snapshots are
/// only ever used through the same one. A Substrate tag belongs to its
/// atespace and is created from with its template.
pub(crate) fn provider_key(provider: &Provider) -> String {
    provider.kind().key()
}

/// Whether the provider could keep a sandbox: why not, otherwise.
pub(crate) fn can_keep(capabilities: &Capabilities) -> Result<(), String> {
    match capabilities.has(branchyard_sandbox::PAUSE) {
        true => Ok(()),
        false => Err("provider can't pause: it does not declare pause".into()),
    }
}

/// How this provider would snapshot a kept sandbox, or why it cannot.
pub(crate) fn method(capabilities: &Capabilities) -> Result<SnapshotMethod, String> {
    if capabilities.has(branchyard_sandbox::LIVE_BRANCH)
        && capabilities.has(branchyard_sandbox::PAUSE)
    {
        return Ok(SnapshotMethod::LiveBranch);
    }
    if checkpoint_guarantee(capabilities).is_some() {
        return Ok(SnapshotMethod::Checkpoint);
    }
    Err(
        "provider can't branch: it declares neither live branch nor a checkpoint it can \
         branch from"
            .into(),
    )
}

/// The checkpoint guarantee the provider can also branch from: a full one
/// when there is one, else the first.
fn checkpoint_guarantee(capabilities: &Capabilities) -> Option<SnapshotGuarantee> {
    let branchable = |g: &&SnapshotGuarantee| capabilities.branch.contains(g);
    let offered = capabilities.checkpoint.iter();
    offered
        .clone()
        .filter(branchable)
        .find(|g| g.scope == SnapshotScope::Full)
        .or_else(|| offered.clone().find(branchable))
        .copied()
}

/// `<stem>-<suffix>` in at most `max` bytes, cutting the stem.
fn named(stem: &str, suffix: &str, max: usize) -> String {
    let room = max.saturating_sub(suffix.len() + 1);
    let stem: String = stem.chars().take(room).collect();
    format!("{}-{suffix}", stem.trim_end_matches('-'))
}

/// A provider for `provider`'s options, as a trait object: the yard's own
/// ([`Yard::use_sandbox_provider`]) or the SDK's for Microsandbox, a signed
/// client for Substrate.
pub(crate) fn open(yard: &Yard, provider: &Provider) -> Result<Arc<dyn SandboxProvider>, String> {
    provider.kind().open(yard)
}

/// A turn's sandbox, and where it came from.
pub(crate) struct Acquired {
    pub name: String,
    pub origin: SandboxOrigin,
}

/// The sandbox a turn wants, for [`acquire`].
pub(crate) struct Wanted<'a> {
    pub record: &'a Record,
    pub fence: &'a Fence,
    /// The provider's key, as recorded on its sandboxes.
    pub key: &'a str,
    pub spec: &'a SandboxSpec,
    pub environment: Option<&'a crate::environments::SandboxEnvironment>,
}

/// Get the turn's sandbox for `record` through `provider`: the branch's
/// kept one, resumed; else one branched from its seed's snapshot; else a
/// fresh one from `spec`. `journal` is called with the sandbox's name
/// before anything is created or resumed. A kept or seeded sandbox that
/// cannot be used is a fallback to a fresh one, with the reason.
pub(crate) fn acquire(
    store: &Store,
    provider: &dyn SandboxProvider,
    journal: &dyn Fn(&str) -> Result<(), String>,
    wanted: Wanted<'_>,
) -> Result<Acquired, String> {
    let Wanted {
        record,
        fence,
        key,
        spec,
        environment,
    } = wanted;
    let mut reasons: Vec<String> = Vec::new();
    // The branch's kept sandbox.
    let kept = store
        .sandboxes()
        .sandboxes(&record.info.name)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|r| r.kind == SandboxKind::Kept && r.incarnation == fence.incarnation)
        .collect::<Vec<_>>();
    for row in kept {
        let taken = store
            .sandboxes()
            .take_sandbox(&row.branch, SandboxKind::Kept, &row.name)
            .map_err(|e| e.to_string())?;
        if taken.is_none() {
            // Evicted under us.
            reasons.push(format!("its kept sandbox {} was evicted", row.name));
            continue;
        }
        if row.provider != key {
            // Kept on a provider the branch no longer uses; that one is
            // not reachable from here.
            reasons.push(format!(
                "its kept sandbox {} is on another provider ({})",
                row.name, row.provider
            ));
            continue;
        }
        match provider.inspect(&row.name) {
            Ok(Some(info)) => {
                journal(&row.name)?;
                let resumed = match info.state {
                    SandboxState::Running => Ok(info),
                    _ => provider.resume(&row.name),
                };
                match resumed {
                    Ok(_) => {
                        return Ok(Acquired {
                            name: row.name,
                            origin: SandboxOrigin::Resumed,
                        })
                    }
                    Err(error) => {
                        branchyard_support::best_effort(
                            "destroy the sandbox",
                            provider.destroy(&row.name),
                        );
                        reasons.push(format!(
                            "its kept sandbox {} could not be resumed: {error}",
                            row.name
                        ));
                    }
                }
            }
            Ok(None) => reasons.push(format!("its kept sandbox {} no longer exists", row.name)),
            Err(error) => reasons.push(format!(
                "its kept sandbox {} could not be inspected: {error}",
                row.name
            )),
        }
    }
    // A snapshot of its source.
    if let Some(seed) = &record.sandbox_seed {
        match branch_from_seed(store, provider, key, seed, spec, journal) {
            Ok(Some(origin)) => {
                return Ok(Acquired {
                    name: spec.name.clone(),
                    origin,
                })
            }
            Ok(None) => {}
            Err(reason) => reasons.push(reason),
        }
    }
    // The prepared environment of its key: setup already ran there.
    if let Some(env) = environment {
        journal(&spec.name)?;
        let used = env.info.key.clone();
        match env
            .info
            .snapshot
            .as_ref()
            .map(|s| branch_environment(provider, s, spec))
        {
            Some(Ok(method)) => {
                return Ok(Acquired {
                    name: spec.name.clone(),
                    origin: SandboxOrigin::Environment {
                        key: env.key.clone(),
                        used: (used != env.key).then_some(used),
                        method,
                        reason: env.reason.clone(),
                    },
                })
            }
            Some(Err(why)) => reasons.push(format!(
                "could not branch from prepared environment {}: {why}",
                &used[..used.len().min(12)]
            )),
            None => {}
        }
    }
    journal(&spec.name)?;
    provider
        .ensure(spec)
        .map_err(|e| format!("could not create sandbox {}: {e}", spec.name))?;
    Ok(Acquired {
        name: spec.name.clone(),
        origin: SandboxOrigin::Fresh {
            reason: (!reasons.is_empty()).then(|| reasons.join("; ")),
        },
    })
}

/// The snapshot `seed` names, among `rows`: at its turn, or else the
/// newest at its commit.
fn seeded_row(rows: Vec<SandboxRow>, seed: &SandboxSeed, key: &str) -> Option<SandboxRow> {
    let mut rows: Vec<SandboxRow> = rows
        .into_iter()
        .filter(|r| {
            r.kind == SandboxKind::Snapshot
                && r.provider == key
                && Detail::parse(&r.detail).commit.as_deref() == Some(seed.commit.as_str())
        })
        .collect();
    rows.sort_by_key(|r| r.turn);
    match seed.turn {
        Some(turn) => rows.into_iter().find(|r| r.turn == Some(turn)),
        None => rows.pop(),
    }
}

/// Branch `spec` from the snapshot `seed` names. `Ok(None)`: there is no
/// such snapshot and nothing asked for one to exist beyond a fork; `Err`:
/// the reason a fresh sandbox is used instead.
fn branch_from_seed(
    store: &Store,
    provider: &dyn SandboxProvider,
    key: &str,
    seed: &SandboxSeed,
    spec: &SandboxSpec,
    journal: &dyn Fn(&str) -> Result<(), String>,
) -> Result<Option<SandboxOrigin>, String> {
    let at = match seed.turn {
        Some(turn) => format!("checkpoint {turn}"),
        None => format!("commit {}", short(&seed.commit)),
    };
    let rows = store
        .sandboxes()
        .sandboxes(&seed.branch)
        .map_err(|e| e.to_string())?;
    let Some(row) = seeded_row(rows, seed, key) else {
        let why = match method(&provider.capabilities()) {
            Err(why) => why,
            Ok(_) => format!("{} has no provider snapshot at {at}", seed.branch),
        };
        return Err(why);
    };
    let detail = Detail::parse(&row.detail);
    let turn = row.turn.unwrap_or_default();
    let method = detail.method.unwrap_or(SnapshotMethod::LiveBranch);
    journal(&spec.name)?;
    let made: Result<SandboxInfo, ProviderError> = match method {
        SnapshotMethod::LiveBranch => provider
            .branch_live(&row.name, std::slice::from_ref(spec))
            .into_iter()
            .next()
            .unwrap_or_else(|| Err(ProviderError::Runtime("no child was made".into()))),
        SnapshotMethod::Checkpoint => {
            let checkpoint = ProviderCheckpoint {
                sandbox: detail.sandbox.clone().unwrap_or_default(),
                reference: row.name.clone(),
                guarantee: detail.guarantee(),
            };
            provider
                .branch(&checkpoint, spec)
                .and_then(|info| match info.state {
                    SandboxState::Running => Ok(info),
                    _ => provider.resume(&spec.name),
                })
        }
    };
    match made {
        Ok(_) => Ok(Some(SandboxOrigin::Branched {
            branch: seed.branch.clone(),
            turn,
            method,
        })),
        Err(error) => {
            branchyard_support::best_effort("destroy the sandbox", provider.destroy(&spec.name));
            Err(format!(
                "could not branch from {}'s snapshot at checkpoint {turn}: {error}",
                seed.branch
            ))
        }
    }
}

fn short(commit: &str) -> &str {
    &commit[..commit.len().min(12)]
}

/// How a running sandbox is snapshotted as a prepared environment, and the
/// name the snapshot will have. Only a live branch leaves the running
/// sandbox undisturbed mid-turn: a provider that can only checkpoint (a
/// Substrate actor is paused, ending its attempt) keeps no environment.
pub(crate) fn plan_environment(
    capabilities: &Capabilities,
    sandbox: &str,
    key: &str,
) -> Result<(SnapshotMethod, String), String> {
    match method(capabilities)? {
        SnapshotMethod::LiveBranch => Ok((
            SnapshotMethod::LiveBranch,
            named(
                &format!("by-env-{}", &key[..key.len().min(12)]),
                &format!("{}", now_ms() % 1_000_000_000),
                63,
            ),
        )),
        SnapshotMethod::Checkpoint => Err(format!(
            "provider can't snapshot a running sandbox without pausing it (it can only \
             checkpoint {sandbox}): prepared sandbox environments need live branch"
        )),
    }
}

/// Snapshot the running `sandbox` into the paused `planned`, the way
/// [`plan_environment`] chose; the handle and its details.
pub(crate) fn take_environment(
    provider: &dyn SandboxProvider,
    sandbox: &str,
    planned: &str,
) -> Result<(String, serde_json::Value), String> {
    let capabilities = provider.capabilities();
    let child = SandboxSpec::new(planned).persist();
    provider
        .branch_live(sandbox, std::slice::from_ref(&child))
        .into_iter()
        .next()
        .unwrap_or_else(|| Err(ProviderError::Runtime("no child was made".into())))
        .and_then(|_| provider.pause(planned))
        .map(|()| {
            let guarantee = capabilities.full_snapshot().unwrap_or(SnapshotGuarantee {
                scope: SnapshotScope::Full,
                consistency: Consistency::Crash,
                locality: Locality::SameHost,
            });
            let detail = Detail {
                method: Some(SnapshotMethod::LiveBranch),
                sandbox: Some(sandbox.to_owned()),
                ..Detail::of(&guarantee)
            };
            (
                planned.to_owned(),
                serde_json::to_value(detail).unwrap_or_default(),
            )
        })
        .map_err(|e| {
            branchyard_support::best_effort("destroy the sandbox", provider.destroy(planned));
            e.to_string()
        })
}

/// Branch `spec` from an environment's snapshot, running.
fn branch_environment(
    provider: &dyn SandboxProvider,
    snapshot: &crate::environments::EnvironmentSnapshot,
    spec: &SandboxSpec,
) -> Result<SnapshotMethod, String> {
    let detail: Detail = serde_json::from_value(snapshot.detail.clone()).unwrap_or_default();
    let made: Result<SandboxInfo, ProviderError> = match snapshot.method {
        SnapshotMethod::LiveBranch => provider
            .branch_live(&snapshot.handle, std::slice::from_ref(spec))
            .into_iter()
            .next()
            .unwrap_or_else(|| Err(ProviderError::Runtime("no child was made".into()))),
        SnapshotMethod::Checkpoint => {
            let checkpoint = ProviderCheckpoint {
                sandbox: detail.sandbox.clone().unwrap_or_default(),
                reference: snapshot.handle.clone(),
                guarantee: detail.guarantee(),
            };
            provider
                .branch(&checkpoint, spec)
                .and_then(|info| match info.state {
                    SandboxState::Running => Ok(info),
                    _ => provider.resume(&spec.name),
                })
        }
    };
    match made {
        Ok(_) => Ok(snapshot.method),
        Err(error) => {
            branchyard_support::best_effort("destroy the sandbox", provider.destroy(&spec.name));
            Err(error.to_string())
        }
    }
}

/// Release an environment's snapshot.
pub(crate) fn release_environment(
    provider: &dyn SandboxProvider,
    snapshot: &crate::environments::EnvironmentSnapshot,
) -> Result<(), String> {
    let detail: Detail = serde_json::from_value(snapshot.detail.clone()).unwrap_or_default();
    let released = match snapshot.method {
        SnapshotMethod::Checkpoint => provider.release_checkpoint(&ProviderCheckpoint {
            sandbox: detail.sandbox.clone().unwrap_or_default(),
            reference: snapshot.handle.clone(),
            guarantee: detail.guarantee(),
        }),
        SnapshotMethod::LiveBranch => provider.destroy(&snapshot.handle),
    };
    released.map_err(|e| e.to_string())
}

/// End a turn's sandbox `name`: park it (pause and record it for the next
/// turn) when the branch keeps its sandbox and the provider can pause, then
/// evict beyond `max_paused`; otherwise destroy it. Returns what to record.
#[allow(clippy::let_underscore_must_use)] // ratchet: branchyard
pub(crate) fn park(
    yard: &Yard,
    record: &Record,
    fence: &Fence,
    provider: &dyn SandboxProvider,
    name: &str,
) -> Vec<Activity> {
    let Some(policy) = lifecycle(record.provider.as_ref()) else {
        return Vec::new();
    };
    let kind = record.provider.as_ref().map_or("", provider_name);
    let key = record
        .provider
        .as_ref()
        .map(provider_key)
        .unwrap_or_default();
    let mut said = Vec::new();
    let destroy = |said: &mut Vec<Activity>, reason: Option<String>| {
        if let Err(error) = provider.destroy(name) {
            said.push(Activity::Warning(format!(
                "could not destroy sandbox {name}: {error}"
            )));
        }
        if let Some(reason) = reason {
            said.push(event(SandboxEvent::NotKept {
                provider: kind.into(),
                sandbox: name.into(),
                reason,
            }));
        }
    };
    if !policy.keep {
        destroy(&mut said, None);
        return said;
    }
    if let Err(why) = can_keep(&provider.capabilities()) {
        destroy(&mut said, Some(why));
        return said;
    }
    let store = yard.store();
    let row = SandboxRow {
        branch: record.info.name.clone(),
        incarnation: fence.incarnation,
        kind: SandboxKind::Kept,
        provider: key.clone(),
        name: name.to_owned(),
        turn: None,
        detail: Detail::default().text(),
        used_ms: now_ms(),
    };
    let intent = json!({ "sandbox": name });
    let begun = store
        .backend()
        .begin_step(fence, fence.turn, STEP_PARK, &intent);
    let parked = begun
        .map_err(|e| e.to_string())
        .and_then(|_| {
            store
                .sandboxes()
                .put_sandbox(&row)
                .map_err(|e| e.to_string())
        })
        .and_then(|()| provider.pause(name).map_err(|e| e.to_string()));
    match parked {
        Ok(()) => {
            let _ =
                store
                    .backend()
                    .finish_step(fence, fence.turn, STEP_PARK, &json!({ "kept": true }));
            said.push(event(SandboxEvent::Kept {
                provider: kind.into(),
                sandbox: name.into(),
            }));
        }
        Err(error) => {
            branchyard_support::best_effort(
                "take the sandbox's row",
                store
                    .sandboxes()
                    .take_sandbox(&row.branch, SandboxKind::Kept, name),
            );
            branchyard_support::best_effort(
                "finish the journal step",
                store.backend().finish_step(
                    fence,
                    fence.turn,
                    STEP_PARK,
                    &json!({ "kept": false }),
                ),
            );
            destroy(&mut said, Some(format!("could not pause it: {error}")));
            return said;
        }
    }
    said.extend(evict(yard, &key, policy.max_paused, &record.info.name));
    said
}

/// Destroy the least recently used kept sandboxes of provider `key` beyond
/// `max`, sparing `except`'s. Each is taken from the store first, so a turn
/// resuming it and this never both act on it.
pub(crate) fn evict(yard: &Yard, key: &str, max: u32, except: &str) -> Vec<Activity> {
    let store = yard.store();
    let Ok(rows) = store.sandboxes().sandboxes_of(SandboxKind::Kept, key) else {
        return Vec::new();
    };
    let mut excess = rows.len().saturating_sub(max as usize);
    let mut said = Vec::new();
    for row in rows {
        if excess == 0 {
            break;
        }
        if row.branch == except {
            continue;
        }
        let Ok(Some(row)) =
            store
                .sandboxes()
                .take_sandbox(&row.branch, SandboxKind::Kept, &row.name)
        else {
            continue;
        };
        excess -= 1;
        let destroyed = store
            .read(&row.branch)
            .ok()
            .and_then(|owner| owner.provider)
            .ok_or_else(|| format!("{} has no provider", row.branch))
            .and_then(|p| open(yard, &p))
            .and_then(|provider| provider.destroy(&row.name).map_err(|e| e.to_string()));
        match destroyed {
            Ok(()) => said.push(event(SandboxEvent::Evicted {
                provider: key.split(':').next().unwrap_or(key).to_owned(),
                sandbox: row.name.clone(),
                branch: row.branch.clone(),
            })),
            Err(error) => said.push(Activity::Warning(format!(
                "could not evict {}'s kept sandbox {}: {error}",
                row.branch, row.name
            ))),
        }
    }
    said
}

/// With the checkpoint of `turn` at `commit` just recorded: when the branch
/// keeps a sandbox, note the turn on it and, when its provider can, take a
/// provider snapshot, keeping the newest `snapshots`. Journaled as
/// [`STEP_SNAPSHOT`] before anything is taken. Returns the snapshot, and
/// what to record.
#[allow(clippy::expect_used)] // ratchet: branchyard
pub(crate) fn snapshot_turn(
    yard: &Yard,
    fence: &Fence,
    record: &Record,
    turn: u32,
    commit: &str,
) -> (Option<SandboxSnapshot>, Vec<Activity>) {
    let mut said = Vec::new();
    let Some(policy) = lifecycle(record.provider.as_ref()) else {
        return (None, said);
    };
    let Some(provider_options) = record.provider.clone() else {
        return (None, said);
    };
    let store = yard.store();
    let key = provider_key(&provider_options);
    let kind = provider_name(&provider_options);
    let Ok(rows) = store.sandboxes().sandboxes(&record.info.name) else {
        return (None, said);
    };
    let Some(mut kept) = rows
        .iter()
        .find(|r| r.kind == SandboxKind::Kept && r.incarnation == fence.incarnation)
        .cloned()
    else {
        return (None, said);
    };
    kept.turn = Some(turn);
    branchyard_support::best_effort(
        "record the sandbox's row",
        store.sandboxes().put_sandbox(&kept),
    );
    if policy.snapshots == 0 {
        return (None, said);
    }
    let provider = match open(yard, &provider_options) {
        Ok(provider) => provider,
        Err(reason) => {
            said.push(event(SandboxEvent::NoSnapshot { turn, reason }));
            return (None, said);
        }
    };
    let capabilities = provider.capabilities();
    let method = match method(&capabilities) {
        Ok(method) => method,
        Err(reason) => {
            said.push(event(SandboxEvent::NoSnapshot { turn, reason }));
            return (None, said);
        }
    };
    let planned = match method {
        SnapshotMethod::LiveBranch => named(&kept.name, &format!("t{turn}"), 63),
        SnapshotMethod::Checkpoint => String::new(),
    };
    let intent =
        json!({ "turn": turn, "method": method, "sandbox": kept.name, "planned": planned });
    if store
        .backend()
        .begin_step(fence, fence.turn, STEP_SNAPSHOT, &intent)
        .is_err()
    {
        return (None, said);
    }
    let taken: Result<(String, Detail), String> = match method {
        SnapshotMethod::LiveBranch => {
            let child = SandboxSpec::new(&planned).persist();
            provider
                .branch_live(&kept.name, std::slice::from_ref(&child))
                .into_iter()
                .next()
                .unwrap_or_else(|| Err(ProviderError::Runtime("no child was made".into())))
                .and_then(|_| provider.pause(&planned))
                .map(|()| {
                    let guarantee = capabilities.full_snapshot().unwrap_or(SnapshotGuarantee {
                        scope: SnapshotScope::Full,
                        consistency: Consistency::Crash,
                        locality: Locality::SameHost,
                    });
                    (planned.clone(), Detail::of(&guarantee))
                })
                .map_err(|e| {
                    branchyard_support::best_effort(
                        "destroy the sandbox",
                        provider.destroy(&planned),
                    );
                    e.to_string()
                })
        }
        SnapshotMethod::Checkpoint => {
            let required =
                checkpoint_guarantee(&capabilities).expect("the method was chosen by it");
            provider
                .checkpoint(&kept.name, &required)
                .map(|checkpoint| {
                    let detail = Detail {
                        sandbox: Some(checkpoint.sandbox.clone()),
                        ..Detail::of(&checkpoint.guarantee)
                    };
                    (checkpoint.reference, detail)
                })
                .map_err(|e| e.to_string())
        }
    };
    let snapshot = match taken {
        Ok((handle, detail)) => {
            let detail = Detail {
                method: Some(method),
                commit: Some(commit.to_owned()),
                ..detail
            };
            let row = SandboxRow {
                branch: record.info.name.clone(),
                incarnation: fence.incarnation,
                kind: SandboxKind::Snapshot,
                provider: key.clone(),
                name: handle.clone(),
                turn: Some(turn),
                detail: detail.text(),
                used_ms: now_ms(),
            };
            branchyard_support::best_effort(
                "record the sandbox's row",
                store.sandboxes().put_sandbox(&row),
            );
            branchyard_support::best_effort(
                "finish the journal step",
                store.backend().finish_step(
                    fence,
                    fence.turn,
                    STEP_SNAPSHOT,
                    &json!({ "handle": handle }),
                ),
            );
            Some(SandboxSnapshot {
                provider: kind.to_owned(),
                handle,
                scope: detail.scope.unwrap_or(SandboxScope::Full),
                consistency: detail.consistency.unwrap_or(SandboxConsistency::Crash),
                method,
            })
        }
        Err(reason) => {
            branchyard_support::best_effort(
                "finish the journal step",
                store.backend().finish_step(
                    fence,
                    fence.turn,
                    STEP_SNAPSHOT,
                    &json!({ "error": reason }),
                ),
            );
            said.push(event(SandboxEvent::NoSnapshot { turn, reason }));
            None
        }
    };
    said.extend(prune(
        &store,
        provider.as_ref(),
        kind,
        &record.info.name,
        policy.snapshots,
    ));
    (snapshot, said)
}

/// Release `branch`'s oldest snapshots beyond the newest `keep`.
fn prune(
    store: &Store,
    provider: &dyn SandboxProvider,
    kind: &str,
    branch: &str,
    keep: u32,
) -> Vec<Activity> {
    let Ok(rows) = store.sandboxes().sandboxes(branch) else {
        return Vec::new();
    };
    let mut snapshots: Vec<SandboxRow> = rows
        .into_iter()
        .filter(|r| r.kind == SandboxKind::Snapshot)
        .collect();
    snapshots.sort_by_key(|r| (r.turn, r.used_ms));
    let excess = snapshots.len().saturating_sub(keep as usize);
    snapshots
        .into_iter()
        .take(excess)
        .filter_map(|row| release(store, provider, kind, row))
        .collect()
}

/// Take `row` and release what it names; what to record.
fn release(
    store: &Store,
    provider: &dyn SandboxProvider,
    kind: &str,
    row: SandboxRow,
) -> Option<Activity> {
    let row = store
        .sandboxes()
        .take_sandbox(&row.branch, row.kind, &row.name)
        .ok()??;
    let detail = Detail::parse(&row.detail);
    let released = match (row.kind, detail.method) {
        (SandboxKind::Snapshot, Some(SnapshotMethod::Checkpoint)) => {
            provider.release_checkpoint(&ProviderCheckpoint {
                sandbox: detail.sandbox.clone().unwrap_or_default(),
                reference: row.name.clone(),
                guarantee: detail.guarantee(),
            })
        }
        _ => provider.destroy(&row.name),
    };
    Some(match (released, row.kind) {
        (Err(error), _) => Activity::Warning(format!(
            "could not release {} sandbox {}: {error}",
            kind, row.name
        )),
        (Ok(()), SandboxKind::Snapshot) => event(SandboxEvent::Released {
            provider: kind.to_owned(),
            handle: row.name,
            turn: row.turn.unwrap_or_default(),
        }),
        (Ok(()), SandboxKind::Kept) => event(SandboxEvent::NotKept {
            provider: kind.to_owned(),
            sandbox: row.name,
            reason: "destroyed with the branch's sandbox state".into(),
        }),
    })
}

/// Destroy `record`'s kept sandbox, if any, because `why`; what to record.
pub(crate) fn discard_kept(yard: &Yard, record: &Record, why: &str) -> Vec<Activity> {
    discard(yard, record, |row| row.kind == SandboxKind::Kept)
        .into_iter()
        .map(|mut a| {
            if let Activity::Sandbox(event) = &mut a {
                if let SandboxEvent::NotKept { reason, .. } = event.as_mut() {
                    *reason = why.to_owned();
                }
            }
            a
        })
        .collect()
}

/// Destroy and release everything `record` holds: its kept sandbox and its
/// snapshots, as its removal does. What to record or report.
pub(crate) fn release_all(yard: &Yard, record: &Record) -> Vec<Activity> {
    discard(yard, record, |_| true)
}

fn discard(yard: &Yard, record: &Record, which: impl Fn(&SandboxRow) -> bool) -> Vec<Activity> {
    let store = yard.store();
    let Ok(rows) = store.sandboxes().sandboxes(&record.info.name) else {
        return Vec::new();
    };
    let rows: Vec<SandboxRow> = rows.into_iter().filter(|r| which(r)).collect();
    if rows.is_empty() {
        return Vec::new();
    }
    let Some(options) = &record.provider else {
        return Vec::new();
    };
    let kind = provider_name(options);
    let provider = match open(yard, options) {
        Ok(provider) => provider,
        Err(error) => {
            return vec![Activity::Warning(format!(
                "could not release {}'s sandboxes: {error}",
                record.info.name
            ))]
        }
    };
    rows.into_iter()
        .filter_map(|row| release(&store, provider.as_ref(), kind, row))
        .collect()
}

/// The setup a branch whose sandbox came from `origin` inherits instead of
/// running its own: the source branch's, when its workspace is ready and
/// runs the same copy and setup. What that setup left in the source's
/// worktree is copied into this one when worktrees are `mounted` (in the
/// sandbox they are not: a Substrate actor's worktree, ignored files
/// included, came with the snapshot).
pub(crate) fn inherited(
    yard: &Yard,
    record: &Record,
    origin: &SandboxOrigin,
    mounted: bool,
) -> Option<crate::workspace::Inherit> {
    let from = match origin {
        SandboxOrigin::Branched { branch, .. } | SandboxOrigin::Prepared { branch, .. } => branch,
        SandboxOrigin::Environment {
            key,
            used,
            method,
            reason,
        } => {
            let used_key = used.as_deref().unwrap_or(key);
            let info = crate::environments::good(&yard.root, used_key)?;
            let mut environment = crate::environments::new_use(
                key,
                match used {
                    Some(_) => crate::environments::EnvironmentOrigin::LastGood,
                    None => crate::environments::EnvironmentOrigin::Restored,
                },
            );
            environment.used = used.clone();
            environment.reason = reason.clone();
            environment.built_by = Some(info.built_by.clone());
            environment.method = serde_json::to_value(method)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned));
            return Some(crate::workspace::Inherit {
                from: format!("environment {}", &used_key[..used_key.len().min(12)]),
                worktree: mounted.then(|| crate::environments::tree(&yard.root, used_key)),
                produced: info.produced,
                environment: Some(Box::new(environment)),
            });
        }
        _ => return None,
    };
    if *from == record.info.name {
        return None;
    }
    let source = yard.store().read(from).ok()?;
    let (ours, theirs) = (record.workspace.as_ref()?, source.workspace.as_ref()?);
    let same = ours.spec.setup == theirs.spec.setup && ours.spec.copy == theirs.spec.copy;
    (theirs.ready && same).then(|| crate::workspace::Inherit {
        from: from.clone(),
        worktree: mounted.then(|| source.info.worktree.clone()),
        produced: theirs.produced.clone(),
        environment: None,
    })
}

/// The seed for a branch created from `source` at `commit` (its checkpoint
/// `turn`, when known), if `source` runs in a sandbox.
pub(crate) fn seed(source: &Record, turn: Option<u32>, commit: &str) -> Option<SandboxSeed> {
    lifecycle(source.provider.as_ref())?;
    Some(SandboxSeed {
        branch: source.info.name.clone(),
        turn,
        commit: commit.to_owned(),
    })
}

/// A step's intent left by a stopped engine: what [`STEP_SNAPSHOT`] or
/// [`STEP_PARK`] was doing, so recovery can clean it up. Returns what to
/// report.
pub(crate) fn recover_steps(
    yard: &Yard,
    record: &Record,
    snapshot: Option<&crate::state::StepRow>,
) -> Option<String> {
    let step = snapshot?;
    if step.outcome.is_some() {
        return None;
    }
    let planned = step.intent.get("planned")?.as_str()?.to_owned();
    if planned.is_empty() {
        return Some(
            "a provider snapshot was being taken; it may have to be released by hand".into(),
        );
    }
    let options = record.provider.as_ref()?;
    let provider = open(yard, options).ok()?;
    branchyard_support::best_effort("destroy the sandbox", provider.destroy(&planned));
    Some(format!(
        "destroyed the unfinished snapshot sandbox {planned}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_fit_and_methods_follow_capabilities() {
        let long = named(&"a".repeat(100), "t12", 63);
        assert_eq!(long.len(), 63);
        assert!(long.ends_with("-t12"));
        let live = Capabilities {
            exec: true,
            pause: true,
            live_branch: true,
            ..Capabilities::default()
        };
        assert_eq!(method(&live), Ok(SnapshotMethod::LiveBranch));
        let disk = SnapshotGuarantee {
            scope: SnapshotScope::Disk,
            consistency: Consistency::Crash,
            locality: Locality::Portable,
        };
        let tagged = Capabilities {
            exec: true,
            pause: true,
            checkpoint: vec![disk],
            branch: vec![disk],
            ..Capabilities::default()
        };
        assert_eq!(method(&tagged), Ok(SnapshotMethod::Checkpoint));
        let plain = Capabilities {
            exec: true,
            ..Capabilities::default()
        };
        assert!(method(&plain).unwrap_err().contains("can't branch"));
        assert!(can_keep(&plain).unwrap_err().contains("can't pause"));
        let detail = Detail::of(&disk);
        assert_eq!(detail.guarantee(), disk);
        assert_eq!(Detail::parse(&detail.text()), detail);
    }

    #[test]
    fn events_describe_the_path_taken() {
        let started = |origin| SandboxEvent::Started {
            provider: "microsandbox".into(),
            sandbox: "by-x-1".into(),
            origin,
        };
        assert_eq!(
            started(SandboxOrigin::Branched {
                branch: "a".into(),
                turn: 3,
                method: SnapshotMethod::LiveBranch
            })
            .describe(),
            "sandbox: branched from a's checkpoint 3 (microsandbox live branch)"
        );
        assert_eq!(
            started(SandboxOrigin::Fresh {
                reason: Some("the provider can't branch a sandbox".into())
            })
            .describe(),
            "sandbox: fresh (the provider can't branch a sandbox)"
        );
        let json = serde_json::to_value(started(SandboxOrigin::Resumed)).unwrap();
        assert_eq!(json["event"], "started");
        assert_eq!(json["origin"]["from"], "resumed");
    }
}
