//! Preparing branches for a turn: `run`, `run_on`, `send` and `fork`.
//!
//! Everything that can be checked without creating anything is checked
//! first: the harness, its executable, the base and the name.

use std::path::PathBuf;

use branchyard_harness::profiles::{self, Profile};
use branchyard_harness::SessionMode;
use branchyard_workspace::Commit;
use serde_json::json;

use crate::delegation::{self, Grant};
use crate::engine::{self, Turn};
use crate::placement;
use crate::record;
use crate::recover;
use crate::state::{now_ms, Lease, Record, Taken};
use crate::{
    git, harness, names, Branch, BranchInfo, BranchStatus, CandidateInfo, Error, NativeSession,
    Provider, Provisioning, TaskOptions, Yard,
};

pub(crate) fn planned_names(
    yard: &Yard,
    prompt: &str,
    options: &TaskOptions,
    harnesses: &[&str],
) -> Result<Vec<String>, Error> {
    names::plan(
        &yard.store(),
        &yard.root,
        options.name.as_deref(),
        prompt,
        harnesses,
    )
}

/// A resolved profile and the command that will launch it.
pub(crate) struct Launch {
    pub profile: &'static Profile,
    pub command: Vec<String>,
}

/// The executable is looked for on this host only when the harness runs
/// here; a sandbox's image must provide it.
pub(crate) fn launch(
    id: Option<&str>,
    command: Option<&[String]>,
    provider: Option<&Provider>,
    unapproved_tools: bool,
) -> Result<Launch, Error> {
    let profile = harness::select(id)?;
    harness::check_approvals(profile, unapproved_tools)?;
    let command = harness::command(profile, command);
    placement::check(provider)?;
    if !placement::sandboxed(provider) {
        harness::check_available(id.unwrap_or(profile.harness), &command)?;
    }
    Ok(Launch { profile, command })
}

pub(crate) fn resolve_base(yard: &Yard, rev: Option<&str>) -> Result<String, Error> {
    let rev = rev.unwrap_or("HEAD");
    yard.repo
        .resolve(rev)
        .map(|commit| commit.0)
        .map_err(|_| Error::Git(format!("{rev:?} does not name a commit")))
}

/// What a new branch starts from.
pub(crate) struct NewBranch<'a> {
    pub name: &'a str,
    pub prompt: &'a str,
    pub profile: &'static Profile,
    pub base: String,
    pub parent: Option<String>,
    pub check: Option<Vec<String>>,
    pub command: Option<Vec<String>>,
    pub home: Option<PathBuf>,
    pub cost_baseline: Option<f64>,
    pub provider: Option<Provider>,
    pub grant: Option<Grant>,
    pub depth: u32,
    pub provision: Option<Provisioning>,
    /// Its workspace lifecycle; see `crate::workspace`.
    pub workspace: Option<crate::WorkspaceSpec>,
}

/// The journaled step that creates a branch's worktree.
const STEP_CREATE: &str = "create";

/// Write the record for a reserved name, take its lease for the first
/// turn, and create its worktree as a journaled step. A worktree that
/// cannot be created leaves the branch `Failed`.
pub(crate) fn create(yard: &Yard, new: NewBranch<'_>) -> Result<(Record, Lease), Error> {
    let store = yard.store();
    let record = new_record(&store, new)?;
    let lease = match store.acquire(&record)? {
        Taken::Granted(lease) => lease,
        Taken::Stale => return Err(Error::Running(record.info.name.clone())),
    };
    materialize(yard, record, lease)
}

/// The record of a branch not yet created, `running` its first turn.
pub(crate) fn new_record(store: &crate::state::Store, new: NewBranch<'_>) -> Result<Record, Error> {
    let branch = names::validate(new.name)?;
    let created_ms = now_ms();
    Ok(Record {
        info: BranchInfo {
            name: new.name.to_owned(),
            git_branch: branch.branch(),
            worktree: store.worktree(new.name),
            prompt: new.prompt.to_owned(),
            harness: new.profile.harness.to_owned(),
            profile: new.profile.id.to_owned(),
            session: None,
            parent: new.parent,
            children: Vec::new(),
            depth: new.depth,
            base: new.base.clone(),
            candidate: None,
            status: BranchStatus::Running,
            turns: 0,
            cost_usd: None,
            created_at: created_ms / 1000,
            stalled: false,
            superseded_by: None,
        },
        created_ms,
        check: new.check,
        command: new.command,
        home: new
            .home
            .or_else(|| placement::private_home(new.provider.as_ref(), store, new.name)),
        cost_baseline: new.cost_baseline,
        provider: new.provider,
        grant: new.grant,
        provision: new.provision,
        bindings: Vec::new(),
        start_base: None,
        checkpoint: Some(0),
        context: None,
        workspace: new.workspace.map(crate::workspace::WorkspaceState::new),
    })
}

/// Create the worktree of a branch whose first turn holds `lease`, from
/// `record.info.base`, as a journaled step. A worktree that cannot be
/// created leaves the branch `Failed`.
pub(crate) fn materialize(
    yard: &Yard,
    mut record: Record,
    lease: Lease,
) -> Result<(Record, Lease), Error> {
    let store = yard.store();
    let branch = names::validate(&record.info.name)?;
    let base = record.info.base.clone();
    let fence = lease.fence().clone();
    let settled = (|| {
        let intent = json!({ "base": base, "worktree": record.info.worktree });
        store
            .backend()
            .begin_step(&fence, fence.turn, STEP_CREATE, &intent)?;
        let created = {
            let _lock = git::lock();
            yard.repo
                .create_branch(&branch, &Commit(base.clone()), &record.info.worktree)
        };
        let outcome = match created {
            Ok(workspace) => {
                record.info.worktree = workspace.path;
                json!({ "worktree": record.info.worktree })
            }
            Err(error) => {
                let reason = format!("could not create the worktree: {error}");
                record.info.status = BranchStatus::Failed {
                    reason: reason.clone(),
                };
                json!({ "error": reason })
            }
        };
        if let Some(home) = &record.home {
            std::fs::create_dir_all(home)?;
        }
        store.write_fenced(&record, &fence)?;
        store
            .backend()
            .finish_step(&fence, fence.turn, STEP_CREATE, &outcome)
    })();
    match settled {
        Ok(()) => Ok((record, lease)),
        Err(error) => {
            record.info.status = BranchStatus::Failed {
                reason: format!("could not create the branch: {error}"),
            };
            let _ = lease.finish(Some(&record), None);
            Err(error)
        }
    }
}

/// Settle a branch that was created but whose turn will not run.
pub(crate) fn abandon(lease: Lease, mut record: Record, why: &Error) {
    record.info.status = BranchStatus::Failed {
        reason: format!("its turn did not start: {why}"),
    };
    let _ = lease.finish(Some(&record), None);
}

pub(crate) fn isolated_home(yard: &Yard, options: &TaskOptions, name: &str) -> Option<PathBuf> {
    options.isolated.then(|| yard.store().home(name))
}

/// Whether a new branch with `options` gets a home of its own.
fn new_home_private(options: &TaskOptions) -> bool {
    options.isolated || placement::sandboxed(options.provider.as_ref())
}

/// The grant for a branch the caller starts: the envelope and any seats,
/// with nothing imposed by a parent. Checks that the MCP server can be
/// found first, and that the seats are a tree whose provisioning can be
/// honored when the branch's home is `private`.
fn root_grant(options: &TaskOptions, private: bool) -> Result<Option<Grant>, Error> {
    let Some(envelope) = &options.delegation else {
        if options.seats.is_some() {
            return Err(seats_need_delegation());
        }
        return Ok(None);
    };
    crate::projection::tools(options)?;
    if let Some(seats) = &options.seats {
        seats.validate()?;
        seats.check_provisioning(private)?;
    }
    Ok(Some(Grant {
        seats: options.seats.clone(),
        ..Grant::root(envelope.clone())
    }))
}

fn seats_need_delegation() -> Error {
    Error::Unsupported("seats need a delegation envelope to spawn them within".into())
}

pub(crate) fn run(yard: &Yard, prompt: &str, options: &TaskOptions) -> Result<Branch, Error> {
    let launch = launch(
        options.harness.as_deref(),
        options.command.as_deref(),
        options.provider.as_ref(),
        options.unapproved_tools,
    )?;
    let grant = root_grant(options, new_home_private(options))?;
    crate::provisioning::check(options.provision.as_ref(), new_home_private(options))?;
    let base = resolve_base(yard, options.base.as_deref())?;
    let store = yard.store();
    let name = names::reserve(&store, &yard.root, options.name.as_deref(), prompt, &[])?.remove(0);
    let record = create(
        yard,
        NewBranch {
            name: &name,
            prompt,
            profile: launch.profile,
            base,
            parent: None,
            check: options.check.clone(),
            command: options.command.clone(),
            home: isolated_home(yard, options, &name),
            cost_baseline: None,
            provider: options.provider.clone(),
            grant,
            depth: 0,
            provision: options.provision.clone(),
            workspace: options.workspace.clone(),
        },
    );
    let (record, lease) = record.inspect_err(|_| store.release(&name))?;
    engine::execute(
        Turn {
            yard,
            record,
            profile: launch.profile,
            command: launch.command,
            mode: SessionMode::Fresh,
            prompt,
            options,
            fork_source: None,
            note: None,
        },
        lease,
    )
}

pub(crate) fn run_on(
    yard: &Yard,
    prompt: &str,
    options: &TaskOptions,
    harnesses: &[&str],
) -> Result<Vec<Branch>, Error> {
    if harnesses.is_empty() {
        return Err(Error::State("run_on needs at least one harness".into()));
    }
    let launches = harnesses
        .iter()
        .map(|id| {
            launch(
                Some(id),
                options.command.as_deref(),
                options.provider.as_ref(),
                options.unapproved_tools,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let grant = root_grant(options, new_home_private(options))?;
    crate::provisioning::check(options.provision.as_ref(), new_home_private(options))?;
    let base = resolve_base(yard, options.base.as_deref())?;
    let store = yard.store();
    let reserved = names::reserve(
        &store,
        &yard.root,
        options.name.as_deref(),
        prompt,
        harnesses,
    )?;
    let mut turns = Vec::new();
    for (index, (name, launch)) in reserved.iter().zip(launches).enumerate() {
        let record = create(
            yard,
            NewBranch {
                name,
                prompt,
                profile: launch.profile,
                base: base.clone(),
                parent: None,
                check: options.check.clone(),
                command: options.command.clone(),
                home: isolated_home(yard, options, name),
                cost_baseline: None,
                provider: options.provider.clone(),
                grant: grant.clone(),
                depth: 0,
                provision: options.provision.clone(),
                workspace: options.workspace.clone(),
            },
        );
        match record {
            Ok((record, lease)) => turns.push((
                Turn {
                    yard,
                    record,
                    profile: launch.profile,
                    command: launch.command,
                    mode: SessionMode::Fresh,
                    prompt,
                    options,
                    fork_source: None,
                    note: None,
                },
                lease,
            )),
            Err(error) => {
                for name in &reserved[index..] {
                    store.release(name);
                }
                for (turn, lease) in turns {
                    abandon(lease, turn.record, &error);
                }
                return Err(error);
            }
        }
    }
    let results: Vec<Result<Branch, Error>> = std::thread::scope(|scope| {
        let handles: Vec<_> = turns
            .into_iter()
            .map(|(turn, lease)| scope.spawn(move || engine::execute(turn, lease)))
            .collect();
        handles
            .into_iter()
            .map(|handle| match handle.join() {
                Ok(result) => result,
                Err(panic) => std::panic::resume_unwind(panic),
            })
            .collect()
    });
    results.into_iter().collect()
}

pub(crate) fn send(
    yard: &Yard,
    name: &str,
    prompt: &str,
    options: &TaskOptions,
) -> Result<Branch, Error> {
    let prepared = prepare_send(yard, name, options, false)?;
    let prompt = prepared.prompt(prompt);
    engine::execute(
        Turn {
            yard,
            record: prepared.record,
            profile: prepared.profile,
            command: prepared.command,
            mode: prepared.mode,
            prompt: &prompt,
            options,
            fork_source: None,
            note: prepared.note,
        },
        prepared.lease,
    )
}

/// A send checked and recorded as running under its lease, ready to
/// execute.
pub(crate) struct Prepared {
    pub record: Record,
    pub lease: Lease,
    pub profile: &'static Profile,
    pub command: Vec<String>,
    pub mode: SessionMode,
    /// Recorded as a warning when the turn starts.
    pub note: Option<String>,
}

impl Prepared {
    /// The prompt to submit: `prompt`, after the summary a rewind left for
    /// a fresh session, if any.
    pub fn prompt(&self, prompt: &str) -> String {
        match &self.record.context {
            Some(context) => crate::checkpoint::compose(context, prompt),
            None => prompt.to_owned(),
        }
    }
}

/// Check that `name` can continue its session, and mark it running under
/// a new lease. Refused while an engine runs a turn on it; a turn left by
/// an engine that stopped is recovered first. `idle` also refuses a branch
/// whose status says it is running a turn.
pub(crate) fn prepare_send(
    yard: &Yard,
    name: &str,
    options: &TaskOptions,
    idle: bool,
) -> Result<Prepared, Error> {
    let store = yard.store();
    recover::stale(yard, name)?;
    let mut record = store.read(name)?;
    // What is still held after recovery belongs to a live engine.
    if store.backend().leases()?.iter().any(|l| l.branch == name) {
        return Err(Error::Running(name.to_owned()));
    }
    if idle && record.info.status == BranchStatus::Running {
        return Err(Error::Running(name.to_owned()));
    }
    match &record.info.status {
        BranchStatus::Waiting => {
            return Err(Error::Denied(format!(
                "{name} is waiting for its prerequisites and has not started; it starts when \
                 they settle, or change its dependencies with a graph proposal"
            )))
        }
        BranchStatus::Blocked { reason } => {
            return Err(Error::Denied(format!(
                "{name} never started and is blocked: {reason}; remove or replace the \
                 dependency with a graph proposal to start it"
            )))
        }
        _ => {}
    }
    let profile = profiles::by_id(&record.info.profile)
        .ok_or_else(|| Error::UnknownHarness(record.info.profile.clone()))?;
    if let Some(id) = &options.harness {
        if harness::select(Some(id))?.id != profile.id {
            return Err(Error::Unsupported(format!(
                "{name} runs {}; a send cannot change its harness",
                profile.id
            )));
        }
    }
    // A branch none of whose turns submitted a prompt has no conversation
    // to continue, such as one cancelled before its harness opened a
    // session: it starts a fresh one, and says so.
    let (mode, note) = match record.info.session.as_deref().and_then(NativeSession::new) {
        Some(session) => {
            if !profile.driver().capabilities().resume {
                return Err(Error::Unsupported(format!(
                    "{} cannot resume a session",
                    profile.id
                )));
            }
            (SessionMode::Resume(session), None)
        }
        None if record.context.is_some() => (
            SessionMode::Fresh,
            Some(format!(
                "{name} was rewound to a checkpoint its harness session cannot continue from; \
                 this turn starts a fresh session whose prompt begins with a summary of the \
                 turns before it"
            )),
        ),
        None if record.checkpoint == Some(0) && record.info.turns > 0 => (
            SessionMode::Fresh,
            Some(format!(
                "{name} was rewound to its base; this turn starts a fresh session with only \
                 this prompt"
            )),
        ),
        None if record.info.turns == 0 => (
            SessionMode::Fresh,
            Some(format!(
                "{name} has no harness session to resume, and no earlier turn submitted a \
                 prompt; this turn starts a fresh session with only this prompt"
            )),
        ),
        None => {
            return Err(Error::Unsupported(format!(
                "{name} has no harness session to resume"
            )))
        }
    };
    if options.command.is_some() {
        record.command = options.command.clone();
    }
    if options.provider.is_some() {
        record.provider = options.provider.clone();
    }
    harness::check_approvals(profile, options.unapproved_tools)?;
    let command = harness::command(profile, record.command.as_deref());
    placement::check(record.provider.as_ref())?;
    if !placement::sandboxed(record.provider.as_ref()) {
        harness::check_available(profile.harness, &command)?;
    } else if record.home.is_none() {
        let home = store.home(name);
        std::fs::create_dir_all(&home)?;
        record.home = Some(home);
    }
    if !record.info.worktree.is_dir() {
        return Err(Error::State(format!(
            "{name}'s worktree {} is missing",
            record.info.worktree.display()
        )));
    }
    if options.check.is_some() {
        record.check = options.check.clone();
    }
    if options.provision.is_some() {
        record.provision = options.provision.clone();
    }
    crate::provisioning::check(record.provision.as_ref(), record.home.is_some())?;
    // A delegated child keeps the envelope and seats its parent gave it.
    if let (Some(envelope), 0) = (&options.delegation, record.info.depth) {
        crate::projection::tools(options)?;
        if let Some(seats) = &options.seats {
            seats.validate()?;
            seats.check_provisioning(record.home.is_some())?;
        }
        record.grant = Some(match record.grant.take() {
            Some(grant) => Grant {
                envelope: envelope.clone(),
                seats: options.seats.clone().or(grant.seats),
                ..grant
            },
            None => Grant {
                seats: options.seats.clone(),
                ..Grant::root(envelope.clone())
            },
        });
    } else if options.seats.is_some() {
        return Err(match record.info.depth {
            0 => seats_need_delegation(),
            _ => Error::Denied(format!(
                "{name} is a delegated child; its seats come from its parent's rig"
            )),
        });
    }
    record.info.status = BranchStatus::Running;
    // A cancel is bound to the turn it was asked of, so one meant for an
    // earlier turn cannot stop this one.
    let lease = match store.acquire(&record)? {
        Taken::Granted(lease) => lease,
        Taken::Stale => return Err(Error::Running(name.to_owned())),
    };
    Ok(Prepared {
        record,
        lease,
        profile,
        command,
        mode,
        note,
    })
}

pub(crate) fn fork(
    yard: &Yard,
    name: &str,
    prompt: &str,
    fresh_session: bool,
    at: Option<u32>,
    options: &TaskOptions,
) -> Result<Branch, Error> {
    let store = yard.store();
    let parent = store.read(name)?;
    // At a checkpoint: its commit, and what the parent's events say of it.
    let checkpoint = match at {
        Some(turn) => {
            let events = record::read(&store, name)?;
            let list = crate::checkpoint::recorded(&events);
            let commit = crate::checkpoint::target_commit(yard, &parent, &list, turn)?;
            Some((turn, commit, list, events))
        }
        None => None,
    };
    let base = match &checkpoint {
        Some((_, commit, _, _)) => commit.clone(),
        None => {
            parent
                .info
                .candidate
                .clone()
                .ok_or_else(|| Error::NoCandidate(name.to_owned()))?
                .commit
        }
    };
    let parent_profile = profiles::by_id(&parent.info.profile)
        .ok_or_else(|| Error::UnknownHarness(parent.info.profile.clone()))?;
    let profile = match &options.harness {
        Some(id) => harness::select(Some(id))?,
        None => parent_profile,
    };
    let same = profile.id == parent_profile.id;
    let session = parent.info.session.as_deref().and_then(NativeSession::new);
    let unsupported = if !same {
        Some(format!(
            "a {} conversation cannot be forked into {}",
            parent_profile.id, profile.id
        ))
    } else if !profile.driver().capabilities().fork {
        Some(format!("{} cannot fork a session", profile.id))
    } else {
        None
    };
    let refusal = match (&unsupported, &session) {
        (Some(reason), _) => Some(reason.clone()),
        (None, None) => Some(format!("{name} has no harness session to fork")),
        (None, Some(_)) => None,
    };
    // At a checkpoint the session forks natively only if it ended there;
    // otherwise the fork starts fresh with a summary, and says so.
    let continuity = checkpoint.as_ref().map(|(turn, commit, list, events)| {
        let supported = match &unsupported {
            Some(reason) => Err(reason.clone()),
            None => Ok(()),
        };
        let continuity = crate::checkpoint::continuity(list, *turn, supported);
        let context = crate::checkpoint::summary(name, events, list, *turn, commit, &continuity);
        (*turn, commit.clone(), continuity, context)
    });
    let mode = match (&continuity, refusal, session) {
        (Some((_, _, crate::SessionContinuity::Native { session }, _)), _, _) => {
            SessionMode::Fork(NativeSession::new(session).ok_or_else(|| {
                Error::State(format!(
                    "{name}'s recorded session {session:?} is not usable"
                ))
            })?)
        }
        (Some(_), _, _) => SessionMode::Fresh,
        (None, None, Some(session)) => SessionMode::Fork(session),
        (None, Some(_), _) | (None, None, None) if fresh_session => SessionMode::Fresh,
        (None, Some(reason), _) => {
            return Err(Error::Unsupported(format!(
                "{reason}; fork with a fresh session to start one on its candidate"
            )))
        }
        (None, None, None) => unreachable!("a missing session is a refusal"),
    };
    let forking = matches!(mode, SessionMode::Fork(_));
    let command = match (&options.command, same) {
        (Some(command), _) => Some(command.clone()),
        (None, true) => parent.command.clone(),
        (None, false) => None,
    };
    harness::check_approvals(profile, options.unapproved_tools)?;
    let launch_command = harness::command(profile, command.as_deref());
    let provider = options.provider.clone().or(parent.provider.clone());
    placement::check(provider.as_ref())?;
    if !placement::sandboxed(provider.as_ref()) {
        harness::check_available(
            options.harness.as_deref().unwrap_or(profile.harness),
            &launch_command,
        )?;
    }
    let private =
        options.isolated || parent.home.is_some() || placement::sandboxed(provider.as_ref());
    let grant = root_grant(options, private)?;
    // Checked before the name is reserved, so a refusal holds no name. The
    // fork has a private home when its parent had one, when it runs
    // isolated, or when its provider is a sandbox (`create` gives it one).
    let provision = options.provision.clone().or(parent.provision.clone());
    crate::provisioning::check(provision.as_ref(), private)?;
    let reserved =
        names::reserve(&store, &yard.root, options.name.as_deref(), prompt, &[])?.remove(0);
    // A forked session lives in the parent's home when it ran isolated.
    let home = match (&parent.home, forking) {
        (Some(home), true) => Some(home.clone()),
        _ if options.isolated || parent.home.is_some() => Some(store.home(&reserved)),
        _ => None,
    };
    let cost_baseline = match (forking, parent.info.cost_usd, parent.cost_baseline) {
        (false, _, _) | (true, None, None) => None,
        (true, own, baseline) => Some(own.unwrap_or(0.0) + baseline.unwrap_or(0.0)),
    };
    let record = create(
        yard,
        NewBranch {
            name: &reserved,
            prompt,
            profile,
            base,
            parent: Some(name.to_owned()),
            check: options.check.clone().or(parent.check.clone()),
            command,
            home,
            cost_baseline,
            provider,
            grant,
            depth: 0,
            provision,
            workspace: options
                .workspace
                .clone()
                .or_else(|| parent.workspace.as_ref().map(|w| w.spec.clone())),
        },
    )
    .inspect_err(|_| store.release(&reserved))?;
    let (record, lease) = record;
    let mut note = None;
    let mut composed = prompt.to_owned();
    if let Some((turn, commit, continuity, context)) = continuity {
        if !continuity.native() {
            note = Some(format!(
                "forked from {name} at checkpoint {turn}; this branch {}",
                continuity.describe()
            ));
        }
        if let Some(context) = &context {
            composed = crate::checkpoint::compose(context, prompt);
        }
        let event = crate::RecordedEvent {
            at_ms: now_ms(),
            activity: crate::Activity::ForkedAt {
                branch: name.to_owned(),
                turn,
                commit,
                session: continuity,
            },
        };
        if let Err(error) = store.append(&reserved, &event, Some(lease.fence())) {
            abandon(lease, record, &error);
            return Err(error);
        }
    }
    engine::execute(
        Turn {
            yard,
            record,
            profile,
            command: launch_command,
            mode,
            prompt: &composed,
            options,
            fork_source: forking.then(|| parent.info.worktree.clone()),
            note,
        },
        lease,
    )
}

/// A new branch from `name`'s latest candidate, always with a fresh session
/// and a generated handoff brief as its first prompt. Marks `name`
/// `superseded_by` the new branch, best-effort. See `docs/lifecycle.md`.
pub(crate) fn reincarnate(yard: &Yard, name: &str, options: &TaskOptions) -> Result<Branch, Error> {
    let store = yard.store();
    let parent = store.read(name)?;
    let candidate = parent
        .info
        .candidate
        .clone()
        .ok_or_else(|| Error::NoCandidate(name.to_owned()))?;
    let parent_profile = profiles::by_id(&parent.info.profile)
        .ok_or_else(|| Error::UnknownHarness(parent.info.profile.clone()))?;
    let profile = match &options.harness {
        Some(id) => harness::select(Some(id))?,
        None => parent_profile,
    };
    let command = match (&options.command, profile.id == parent_profile.id) {
        (Some(command), _) => Some(command.clone()),
        (None, true) => parent.command.clone(),
        (None, false) => None,
    };
    harness::check_approvals(profile, options.unapproved_tools)?;
    let launch_command = harness::command(profile, command.as_deref());
    let provider = options.provider.clone().or(parent.provider.clone());
    placement::check(provider.as_ref())?;
    if !placement::sandboxed(provider.as_ref()) {
        harness::check_available(
            options.harness.as_deref().unwrap_or(profile.harness),
            &launch_command,
        )?;
    }
    let private = options.isolated || placement::sandboxed(provider.as_ref());
    let grant = root_grant(options, private)?;
    let provision = options.provision.clone().or(parent.provision.clone());
    crate::provisioning::check(provision.as_ref(), private)?;
    let brief = handoff_brief(&store, name, &parent, &candidate, profile, parent_profile);
    let reserved = names::reserve(
        &store,
        &yard.root,
        options.name.as_deref(),
        &parent.info.prompt,
        &[],
    )?
    .remove(0);
    // A reincarnation never continues a session, so it never shares its
    // parent's home unless isolation or a sandbox provider asks for one.
    let home = private.then(|| store.home(&reserved));
    let record = create(
        yard,
        NewBranch {
            name: &reserved,
            prompt: &brief,
            profile,
            base: candidate.commit,
            parent: Some(name.to_owned()),
            check: options.check.clone().or(parent.check.clone()),
            command,
            home,
            cost_baseline: None,
            provider,
            grant,
            depth: 0,
            provision,
            workspace: options
                .workspace
                .clone()
                .or_else(|| parent.workspace.as_ref().map(|w| w.spec.clone())),
        },
    )
    .inspect_err(|_| store.release(&reserved))?;
    let (record, lease) = record;
    let new_name = record.info.name.clone();
    // Best-effort and informational only: never blocks the new branch from
    // starting, and races harmlessly with a concurrent write to the old
    // branch the way any out-of-turn `store.write` does.
    if let Ok(mut old) = store.read(name) {
        old.info.superseded_by = Some(new_name);
        let _ = store.write(&old);
    }
    engine::execute(
        Turn {
            yard,
            record,
            profile,
            command: launch_command,
            mode: SessionMode::Fresh,
            prompt: &brief,
            options,
            fork_source: None,
            note: Some(format!(
                "reincarnated from {name} with a fresh session and a handoff brief"
            )),
        },
        lease,
    )
}

/// The first prompt for a reincarnated branch: the original task, turns so
/// far, its last message, the latest candidate's diffstat, and why it was
/// reincarnated.
fn handoff_brief(
    store: &crate::state::Store,
    name: &str,
    parent: &Record,
    candidate: &CandidateInfo,
    profile: &Profile,
    parent_profile: &Profile,
) -> String {
    let events = record::read(store, name).unwrap_or_default();
    let last = delegation::last_message(&events);
    let mut brief = format!(
        "{name} is being reincarnated: continue its work in a fresh session.\n\n\
         ## Original task\n{}\n\n\
         ## Progress so far\n{} turn(s) completed with {}.\n\n\
         ## Latest candidate\n{} ({} file(s) changed, +{} -{})\n",
        parent.info.prompt,
        parent.info.turns,
        parent_profile.id,
        candidate.commit,
        candidate.files_changed,
        candidate.insertions,
        candidate.deletions,
    );
    if profile.id != parent_profile.id {
        brief.push_str(&format!(
            "\nThis run switches harness from {} to {}.\n",
            parent_profile.id, profile.id
        ));
    }
    if !last.is_empty() {
        brief.push_str(&format!("\n## Its last message\n{last}\n"));
    }
    match &parent.info.status {
        BranchStatus::Failed { reason } => brief.push_str(&format!(
            "\n## Why it was reincarnated\nIts last turn failed: {reason}\n"
        )),
        BranchStatus::BudgetExceeded { limit } => brief.push_str(&format!(
            "\n## Why it was reincarnated\nIts last turn hit its {limit} limit.\n"
        )),
        BranchStatus::Interrupted => {
            brief.push_str("\n## Why it was reincarnated\nIts last turn was interrupted.\n")
        }
        _ => {}
    }
    brief.push_str("\nPick up from the candidate above and continue the task.");
    brief
}
