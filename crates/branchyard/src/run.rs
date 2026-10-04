//! Preparing branches for a turn: `run`, `run_on`, `send` and `fork`.
//!
//! Everything that can be checked without creating anything is checked
//! first: the harness, its executable, the base and the name.

use std::path::PathBuf;

use branchyard_harness::profiles::{self, Profile};
use branchyard_harness::SessionMode;
use branchyard_sandbox::SandboxSpec;
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
    yard: &Yard,
    id: Option<&str>,
    command: Option<&[String]>,
    provider: Option<&Provider>,
    unapproved_tools: bool,
) -> Result<Launch, Error> {
    let profile = harness::select(id)?;
    harness::check_approvals(profile, unapproved_tools)?;
    let command = harness::command(profile, command);
    placement::check(yard, provider)?;
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
    /// The provider snapshot its first sandbox should come from; see
    /// `crate::snapshots`.
    pub seed: Option<crate::snapshots::SandboxSeed>,
    /// Who it acts for at the connector gateway; see `crate::connectors`.
    pub actor: Option<crate::connectors::Actor>,
    /// The task it is an attempt of, recorded once its record is written;
    /// `None` for a branch that is part of another's work (a delegated
    /// child, an adopted worktree).
    pub task: Option<crate::tasks::Joining>,
}

/// The journaled step that creates a branch's worktree.
const STEP_CREATE: &str = "create";

/// Write the record for a reserved name, take its lease for the first
/// turn, and create its worktree as a journaled step. A worktree that
/// cannot be created leaves the branch `Failed`.
pub(crate) fn create(yard: &Yard, mut new: NewBranch<'_>) -> Result<(Record, Lease), Error> {
    let store = yard.store();
    let joining = new.task.take();
    let record = new_record(&store, new)?;
    if let Some(joining) = &joining {
        crate::tasks::attach(&yard.root, joining, &record.info.name)?;
    }
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
        sandbox_seed: new.seed,
        actor: new.actor,
        plan: None,
        goal: None,
    })
}

/// Create the worktree of a branch whose first turn holds `lease`, from
/// `record.info.base`, as a journaled step: a ready slot of its warm pool
/// when it has one (`crate::pool`), else a new worktree. A worktree that
/// cannot be created leaves the branch `Failed`.
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
        let started = std::time::Instant::now();
        let pooled = crate::pool::eligible(&record);
        let taken = pooled.then(|| crate::pool::take(yard, &record, &base));
        let mut slot = None;
        let mut missed = None;
        let created = match taken {
            Some(Ok(taken)) => {
                let adopted = {
                    let _lock = git::lock();
                    yard.repo.adopt_worktree(
                        &branch,
                        &Commit(base.clone()),
                        std::path::Path::new(&taken.row.path),
                        &record.info.worktree,
                    )
                };
                match adopted {
                    Ok(workspace) => {
                        crate::pool::settle(yard, &taken);
                        slot = Some(taken);
                        Ok(workspace)
                    }
                    Err(error) => {
                        crate::pool::abandon(yard, &taken);
                        missed = Some(format!("could not take slot {}: {error}", taken.row.id));
                        let _lock = git::lock();
                        yard.repo.create_branch(
                            &branch,
                            &Commit(base.clone()),
                            &record.info.worktree,
                        )
                    }
                }
            }
            taken => {
                missed = taken.and_then(Result::err);
                let _lock = git::lock();
                yard.repo
                    .create_branch(&branch, &Commit(base.clone()), &record.info.worktree)
            }
        };
        if pooled {
            let used = crate::PoolUse {
                slot: slot.as_ref().map(|t| t.row.id.clone()),
                reason: match &slot {
                    Some(t) if t.behind > 0 => Some(format!(
                        "brought forward {} commit{}",
                        t.behind,
                        if t.behind == 1 { "" } else { "s" }
                    )),
                    Some(_) => None,
                    None => missed,
                },
                requested_ms: record.created_ms,
                worktree_ms: started.elapsed().as_millis() as u64,
            };
            let claim = match &slot {
                Some(taken) => crate::pool::claim_of(taken, used),
                None => crate::pool::PoolClaim {
                    used,
                    key: None,
                    method: None,
                    shared: Vec::new(),
                    produced: Vec::new(),
                },
            };
            if let Some(workspace) = record.workspace.as_mut() {
                workspace.pool = Some(claim);
            }
        }
        let outcome = match created {
            Ok(workspace) => {
                record.info.worktree = workspace.path;
                // A task's own repository keeps large files as pointers.
                if let Err(error) = crate::tasks::after_checkout(yard, &record.info.worktree) {
                    record.info.status = BranchStatus::Failed {
                        reason: format!("could not restore its large files: {error}"),
                    };
                }
                match &slot {
                    Some(taken) => json!({
                        "worktree": record.info.worktree,
                        "pool_slot": taken.row.id,
                    }),
                    None => json!({ "worktree": record.info.worktree }),
                }
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
            branchyard_support::best_effort("lease.finish", lease.finish(Some(&record), None));
            Err(error)
        }
    }
}

/// Settle a branch that was created but whose turn will not run.
pub(crate) fn abandon(lease: Lease, mut record: Record, why: &Error) {
    record.info.status = BranchStatus::Failed {
        reason: format!("its turn did not start: {why}"),
    };
    branchyard_support::best_effort("lease.finish", lease.finish(Some(&record), None));
}

/// Start a new top-level branch's plan and goal, as `options` ask, under
/// its first turn's lease. A branch that cannot have them is abandoned.
pub(crate) fn begin_new(
    yard: &Yard,
    record: &mut Record,
    lease: &Lease,
    profile: &'static Profile,
    options: &TaskOptions,
) -> Result<(), Error> {
    if options.plan {
        crate::plan::begin(yard, record, lease.fence(), profile)?;
    }
    if let Some(goal) = &options.goal {
        crate::goal::begin(yard, record, lease.fence(), goal)?;
    }
    Ok(())
}

/// The first turn's prompt: the task, or the planning prompt around it.
pub(crate) fn first_prompt(prompt: &str, options: &TaskOptions) -> String {
    match options.plan {
        true => crate::plan::planning_prompt(prompt),
        false => prompt.to_owned(),
    }
}

/// Refuse a plan or goal that cannot work, before anything is created.
fn check_plan_and_goal(profile: &'static Profile, options: &TaskOptions) -> Result<(), Error> {
    if options.plan {
        crate::plan::check_profile(profile)?;
    }
    if options
        .goal
        .as_ref()
        .is_some_and(|g| g.text.trim().is_empty())
    {
        return Err(Error::Unsupported("a goal needs text".into()));
    }
    Ok(())
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
        yard,
        options.harness.as_deref(),
        options.command.as_deref(),
        options.provider.as_ref(),
        options.unapproved_tools,
    )?;
    check_plan_and_goal(launch.profile, options)?;
    let grant = root_grant(options, new_home_private(options))?;
    crate::provisioning::check(options.provision.as_ref(), new_home_private(options))?;
    crate::egress::check(options.provision.as_ref(), options.provider.as_ref())?;
    let base = resolve_base(yard, options.base.as_deref())?;
    let store = yard.store();
    let joining = crate::tasks::joining(yard, prompt, options, "run")?;
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
            seed: None,
            actor: options.actor.clone(),
            task: Some(joining),
        },
    );
    let (mut record, lease) = record.inspect_err(|_| store.release(&name))?;
    if let Err(error) = begin_new(yard, &mut record, &lease, launch.profile, options) {
        abandon(lease, record, &error);
        return Err(error);
    }
    let first = first_prompt(prompt, options);
    engine::execute(
        Turn {
            yard,
            record,
            profile: launch.profile,
            command: launch.command,
            mode: SessionMode::Fresh,
            prompt: &first,
            options,
            fork_source: None,
            note: None,
            sandbox: Default::default(),
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
                yard,
                Some(id),
                options.command.as_deref(),
                options.provider.as_ref(),
                options.unapproved_tools,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    for launch in &launches {
        check_plan_and_goal(launch.profile, options)?;
    }
    let grant = root_grant(options, new_home_private(options))?;
    crate::provisioning::check(options.provision.as_ref(), new_home_private(options))?;
    crate::egress::check(options.provision.as_ref(), options.provider.as_ref())?;
    let base = resolve_base(yard, options.base.as_deref())?;
    let store = yard.store();
    let origin = if harnesses.len() > 1 { "fan" } else { "run" };
    let joining = crate::tasks::joining(yard, prompt, options, origin)?;
    let reserved = names::reserve(
        &store,
        &yard.root,
        options.name.as_deref(),
        prompt,
        harnesses,
    )?;
    let first = first_prompt(prompt, options);
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
                seed: None,
                actor: options.actor.clone(),
                task: Some(joining.clone()),
            },
        )
        .and_then(|(mut record, lease)| {
            match begin_new(yard, &mut record, &lease, launch.profile, options) {
                Ok(()) => Ok((record, lease)),
                Err(error) => {
                    abandon(lease, record, &error);
                    Err(error)
                }
            }
        });
        match record {
            Ok((record, lease)) => turns.push((
                Turn {
                    yard,
                    record,
                    profile: launch.profile,
                    command: launch.command,
                    mode: SessionMode::Fresh,
                    prompt: &first,
                    options,
                    fork_source: None,
                    note: None,
                    sandbox: Default::default(),
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
    prepare_fan(yard, options, &mut turns);
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

/// One branch of [`run_attempts`]: what differs from the task's options.
pub(crate) struct AttemptSpec {
    /// The name's suffix, as a fan's harness: `<name>-<label>`. `None` for
    /// a single branch named as `run` names it.
    pub label: Option<String>,
    pub harness: Option<String>,
    /// Replaces the task's command when set.
    pub command: Option<Vec<String>>,
    /// Replaces the task's provisioning.
    pub provision: Option<Provisioning>,
    /// Replaces the task's budget when set.
    pub budget: Option<crate::Budget>,
    /// Recorded on the branch before its first turn.
    pub events: Vec<crate::Activity>,
}

/// `run`, or `run_on` with a harness, command, provisioning and budget per
/// branch, recording each spec's events before its turn: a routed run or
/// fan (`crate::fleet`). Branches with labels run in parallel, as a fan.
pub(crate) fn run_attempts(
    yard: &Yard,
    prompt: &str,
    options: &TaskOptions,
    specs: Vec<AttemptSpec>,
) -> Result<Vec<Branch>, Error> {
    if specs.is_empty() {
        return Err(Error::State(
            "run_attempts needs at least one attempt".into(),
        ));
    }
    let per: Vec<TaskOptions> = specs
        .iter()
        .map(|spec| TaskOptions {
            harness: spec.harness.clone().or(options.harness.clone()),
            command: spec.command.clone().or(options.command.clone()),
            provision: spec.provision.clone(),
            budget: spec.budget.clone().unwrap_or(options.budget.clone()),
            ..options.clone()
        })
        .collect();
    let launches = per
        .iter()
        .map(|o| {
            launch(
                yard,
                o.harness.as_deref(),
                o.command.as_deref(),
                o.provider.as_ref(),
                o.unapproved_tools,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    for launch in &launches {
        check_plan_and_goal(launch.profile, options)?;
    }
    let grant = root_grant(options, new_home_private(options))?;
    for o in &per {
        crate::provisioning::check(o.provision.as_ref(), new_home_private(o))?;
        crate::egress::check(o.provision.as_ref(), o.provider.as_ref())?;
    }
    let first = first_prompt(prompt, options);
    let base = resolve_base(yard, options.base.as_deref())?;
    let store = yard.store();
    let labels: Vec<&str> = specs.iter().filter_map(|s| s.label.as_deref()).collect();
    if !labels.is_empty() && labels.len() != specs.len() {
        return Err(Error::State(
            "run_attempts needs a label for every attempt or for none".into(),
        ));
    }
    if labels.is_empty() && specs.len() > 1 {
        return Err(Error::State("several attempts need labels".into()));
    }
    let origin = if specs.len() > 1 { "fan" } else { "run" };
    let joining = crate::tasks::joining(yard, prompt, options, origin)?;
    let reserved = names::reserve(&store, &yard.root, options.name.as_deref(), prompt, &labels)?;
    let mut turns = Vec::new();
    for (index, ((name, launch), (spec, o))) in reserved
        .iter()
        .zip(launches)
        .zip(specs.iter().zip(&per))
        .enumerate()
    {
        let created = create(
            yard,
            NewBranch {
                name,
                prompt,
                profile: launch.profile,
                base: base.clone(),
                parent: None,
                check: options.check.clone(),
                command: o.command.clone(),
                home: isolated_home(yard, options, name),
                cost_baseline: None,
                provider: options.provider.clone(),
                grant: grant.clone(),
                depth: 0,
                provision: o.provision.clone(),
                workspace: options.workspace.clone(),
                seed: None,
                actor: options.actor.clone(),
                task: Some(joining.clone()),
            },
        )
        .and_then(|(mut record, lease)| {
            for activity in &spec.events {
                let event = crate::RecordedEvent {
                    at_ms: now_ms(),
                    activity: activity.clone(),
                };
                if let Err(error) = store.append(name, &event, Some(lease.fence())) {
                    abandon(lease, record, &error);
                    return Err(error);
                }
            }
            if let Err(error) = begin_new(yard, &mut record, &lease, launch.profile, options) {
                abandon(lease, record, &error);
                return Err(error);
            }
            Ok((record, lease))
        });
        match created {
            Ok((record, lease)) => turns.push((
                Turn {
                    yard,
                    record,
                    profile: launch.profile,
                    command: launch.command,
                    mode: SessionMode::Fresh,
                    prompt: &first,
                    options: o,
                    fork_source: None,
                    note: None,
                    sandbox: Default::default(),
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
    prepare_fan(yard, options, &mut turns);
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

/// A fan of sandboxed branches that share a provider which can live-branch
/// and have setup to run: the setup runs once, in a sandbox prepared on the
/// first branch's worktree, which is paused and live-branched into one
/// sandbox per branch, each rebound to its own worktree and home. The first
/// branch's worktree then has what setup produced; each other branch gets a
/// copy when its turn starts ([`crate::snapshots::inherited`]). The
/// prepared sandbox is destroyed. Anything that does not work leaves the
/// branch on the ordinary path, with the reason recorded when its sandbox
/// starts. See `docs/sandbox-snapshots.md`.
fn prepare_fan(yard: &Yard, options: &TaskOptions, turns: &mut [(Turn<'_>, Lease)]) {
    use crate::placement::SandboxPlan;
    use crate::snapshots::{SandboxOrigin, SnapshotMethod};
    let setup = options
        .workspace
        .as_ref()
        .is_some_and(|w| !w.setup.is_empty());
    if turns.len() < 2 || !setup || yard.hub.scripts_denied() {
        return;
    }
    let fresh = |turns: &mut [(Turn<'_>, Lease)], why: String| {
        for (turn, _) in turns.iter_mut() {
            turn.sandbox = SandboxPlan::Fresh(format!("fan setup runs in each branch: {why}"));
        }
    };
    let Ok(Some((mut spec, provider))) = crate::placement::fan_spec(yard, &turns[0].0.record)
    else {
        return;
    };
    // A prepared environment of the fan's key already holds its setup:
    // every branch branches from it instead.
    let prepared = options.workspace.as_ref().and_then(|w| {
        let key = turns[0]
            .0
            .record
            .provider
            .as_ref()
            .map(crate::snapshots::provider_key)?;
        crate::environments::for_sandbox(&yard.root, w, &turns[0].0.record.info.worktree, &key)
    });
    if prepared.is_some() {
        return;
    }
    let capabilities = provider.capabilities();
    if !capabilities.has(branchyard_sandbox::LIVE_BRANCH) {
        return fresh(
            turns,
            "provider can't live-branch: it does not declare live branch".into(),
        );
    }
    let store = yard.store();
    let first = turns[0].0.record.info.name.clone();
    spec.name = format!("{}-fan", spec.name.chars().take(120).collect::<String>());
    // Attached to this process where the provider ties sandboxes to it, so
    // an engine that stops mid-fan leaves it nothing to clean up.
    spec.persist = false;
    if let Err(error) = provider.ensure(&spec) {
        branchyard_support::best_effort("provider.destroy", provider.destroy(&spec.name));
        return fresh(
            turns,
            format!("could not create the prepared sandbox: {error}"),
        );
    }
    let prepared = {
        let (turn, lease) = &mut turns[0];
        let mut recorder =
            crate::record::Recorder::fenced(&store, lease.fence(), turn.options.observer.clone());
        let runner = crate::workspace::Runner::Sandbox {
            provider: provider.as_ref(),
            name: &spec.name,
            cwd: crate::placement::WORKSPACE.to_owned(),
            mounted: true,
        };
        crate::workspace::prepare(
            yard,
            &mut turn.record,
            lease.fence(),
            &mut recorder,
            &|| lease.lost(),
            &runner,
            None,
        )
    };
    match prepared {
        Ok(Ok(())) => {}
        Ok(Err(why)) => {
            branchyard_support::best_effort("provider.destroy", provider.destroy(&spec.name));
            return fresh(
                turns,
                format!("its setup failed in the prepared sandbox: {why}"),
            );
        }
        Err(error) => {
            branchyard_support::best_effort("provider.destroy", provider.destroy(&spec.name));
            return fresh(turns, format!("its setup could not run: {error}"));
        }
    }
    // Paused, every child is captured at the same point.
    if capabilities.has(branchyard_sandbox::PAUSE) {
        let _ = provider.pause(&spec.name);
    }
    let mut children = Vec::new();
    for (turn, lease) in turns.iter() {
        let child = match crate::placement::fan_spec(yard, &turn.record) {
            // Handed to each branch's turn, which runs through a provider
            // value of its own: it must outlive this one, which destroys
            // what it holds when dropped. The turn destroys or keeps it.
            Ok(Some((child, _))) => SandboxSpec {
                image: None,
                resources: Default::default(),
                persist: true,
                ..child
            },
            _ => SandboxSpec::new(format!("{}-unplaced", turn.record.info.name)),
        };
        let _ = crate::placement::journal_handed(yard, &turn.record, lease.fence(), &child.name);
        children.push(child);
    }
    let made = provider.branch_live(&spec.name, &children);
    branchyard_support::best_effort("provider.destroy", provider.destroy(&spec.name));
    for (((turn, _), child), made) in turns.iter_mut().zip(children).zip(made) {
        turn.sandbox = match made {
            Ok(_) => SandboxPlan::Handed {
                name: child.name,
                origin: SandboxOrigin::Prepared {
                    branch: first.clone(),
                    method: SnapshotMethod::LiveBranch,
                },
            },
            Err(error) => SandboxPlan::Fresh(format!(
                "could not branch the fan's prepared sandbox: {error}"
            )),
        };
    }
    // Every other branch inherits the first one's setup now, before any
    // harness runs: what it produced is copied into each worktree.
    let (source, rest) = turns.split_first_mut().expect("at least two");
    let produced = source
        .0
        .record
        .workspace
        .as_ref()
        .map(|w| w.produced.clone())
        .unwrap_or_default();
    for (turn, lease) in rest {
        let SandboxPlan::Handed { name, .. } = &turn.sandbox else {
            continue;
        };
        let inherit = crate::workspace::Inherit {
            from: first.clone(),
            worktree: Some(source.0.record.info.worktree.clone()),
            produced: produced.clone(),
            environment: None,
        };
        let runner = crate::workspace::Runner::Sandbox {
            provider: provider.as_ref(),
            name,
            cwd: crate::placement::WORKSPACE.to_owned(),
            mounted: true,
        };
        let mut recorder =
            crate::record::Recorder::fenced(&store, lease.fence(), turn.options.observer.clone());
        let _ = crate::workspace::prepare(
            yard,
            &mut turn.record,
            lease.fence(),
            &mut recorder,
            &|| lease.lost(),
            &runner,
            Some(&inherit),
        );
    }
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
            sandbox: Default::default(),
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
    prepare_send_with(yard, name, options, idle, false)
}

/// [`prepare_send`]; `plan` also takes a branch awaiting plan approval, as
/// an approval or a re-plan does (`crate::plan`). A plain send refuses it.
pub(crate) fn prepare_send_with(
    yard: &Yard,
    name: &str,
    options: &TaskOptions,
    idle: bool,
    plan: bool,
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
        BranchStatus::AwaitingPlanApproval if !plan => {
            return Err(Error::Denied(format!(
                "{name}'s plan awaits approval; approve it (by plan approve {name}) or reject \
                 it (by plan reject {name} [--replan]) before sending it anything"
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
    placement::check(yard, record.provider.as_ref())?;
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
    if let Some(asked) = &options.provision {
        let mut spec = same_model(name, &record, asked.clone())?;
        // A delegated child's connectors stay within its parent's grant,
        // whoever sends it.
        // So does its network policy.
        if let (Some(parent), true) = (&record.info.parent, record.info.depth > 0) {
            let parent_spec = store.read(parent).ok().and_then(|p| p.provision);
            let parent_grant = parent_spec
                .as_ref()
                .map(|p| p.connectors.clone())
                .unwrap_or_default();
            spec.connectors =
                branchyard_provision::connectors::narrow(Some(&spec.connectors), &parent_grant)
                    .map_err(|why| Error::Denied(format!("{name}: {why}")))?;
            let asked = spec
                .network
                .clone()
                .or_else(|| record.provision.as_ref().and_then(|p| p.network.clone()));
            spec.network = branchyard_provision::network::narrow(
                asked.as_ref(),
                parent_spec.as_ref().and_then(|p| p.network.as_ref()),
            )
            .map_err(|why| Error::Denied(format!("{name}: {why}")))?;
            // And its models.
            let asked = spec
                .models
                .clone()
                .or_else(|| record.provision.as_ref().and_then(|p| p.models.clone()));
            spec.models = branchyard_provision::models::narrow(
                asked.as_ref(),
                parent_spec.as_ref().and_then(|p| p.models.as_ref()),
            )
            .map_err(|why| Error::Denied(format!("{name}: {why}")))?;
            // And its approvals, which may only be stricter.
            spec.approvals = branchyard_provision::approvals::narrow(
                spec.approvals.as_ref(),
                parent_spec.as_ref().and_then(|p| p.approvals.as_ref()),
            );
        }
        record.provision = Some(spec);
    }
    crate::provisioning::check(record.provision.as_ref(), record.home.is_some())?;
    crate::egress::check(record.provision.as_ref(), record.provider.as_ref())?;
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

/// A send's provisioning keeps the session on the model and reasoning
/// effort it started with: unset, they are the branch's; a different model
/// is refused once a turn has run, since switching models mid-session is
/// not a continuation (reincarnate or fork with a fresh session instead).
fn same_model(name: &str, record: &Record, mut asked: Provisioning) -> Result<Provisioning, Error> {
    let had = record.provision.as_ref();
    match (had.and_then(|p| p.model.as_deref()), asked.model.as_deref()) {
        (Some(have), Some(want)) if have != want && record.info.turns > 0 => {
            return Err(Error::Unsupported(format!(
                "{name}'s session runs model {have}; a send cannot switch it to {want} \
                 mid-branch (reincarnate it with the other model instead)"
            )))
        }
        (Some(have), None) => asked.model = Some(have.to_owned()),
        _ => {}
    }
    if asked.effort.is_none() {
        asked.effort = had.and_then(|p| p.effort);
    }
    // A branch on the model gateway stays on it: its key never enters the
    // harness's reach, and its cost stays metered.
    if asked.models.is_none() {
        asked.models = had.and_then(|p| p.models.clone());
    }
    // So does a branch's approval policy, unless a send replaces it.
    if asked.approvals.is_none() {
        asked.approvals = had.and_then(|p| p.approvals.clone());
    }
    Ok(asked)
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
    placement::check(yard, provider.as_ref())?;
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
    crate::egress::check(provision.as_ref(), provider.as_ref())?;
    let joining = crate::tasks::joining_fork(yard, name, at, prompt, options)?;
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
    // The fork's first sandbox, from the parent's provider snapshot at its
    // base when there is one (`crate::snapshots`), on the same provider.
    let seed = match provider == parent.provider {
        true => crate::snapshots::seed(&parent, at.or(parent.checkpoint.filter(|n| *n > 0)), &base),
        false => None,
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
            seed,
            actor: options.actor.clone().or(parent.actor.clone()),
            task: Some(joining),
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
            sandbox: Default::default(),
        },
        lease,
    )
}

/// A new branch from `name`'s latest candidate, always with a fresh session
/// and a generated handoff brief as its first prompt. Marks `name`
/// `superseded_by` the new branch, best-effort. See `docs/lifecycle.md`.
pub(crate) fn reincarnate(yard: &Yard, name: &str, options: &TaskOptions) -> Result<Branch, Error> {
    reincarnate_with(yard, name, options, Reincarnation::default())
}

/// How [`reincarnate_with`] differs from a plain reincarnation.
#[derive(Default)]
pub(crate) struct Reincarnation {
    /// Start from the branch's base when it has no candidate, with its
    /// original prompt when no turn of it submitted one, instead of
    /// refusing.
    pub from_base: bool,
    /// What the new name is a slug of, instead of the original prompt.
    pub stem: Option<String>,
    /// Recorded on the new branch before its turn.
    pub events: Vec<crate::Activity>,
    /// Why, for the handoff brief when the status does not say it.
    pub why: Option<String>,
}

/// [`reincarnate`], as a failover (`crate::fleet`) needs it.
pub(crate) fn reincarnate_with(
    yard: &Yard,
    name: &str,
    options: &TaskOptions,
    how: Reincarnation,
) -> Result<Branch, Error> {
    let store = yard.store();
    let parent = store.read(name)?;
    let candidate = match (parent.info.candidate.clone(), how.from_base) {
        (Some(candidate), _) => Some(candidate),
        (None, true) => None,
        (None, false) => return Err(Error::NoCandidate(name.to_owned())),
    };
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
    placement::check(yard, provider.as_ref())?;
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
    let brief = match &candidate {
        Some(candidate) => handoff_brief(&store, name, &parent, candidate, profile, parent_profile),
        // Nothing to hand off: no turn of it submitted a prompt.
        None if parent.info.turns == 0 => parent.info.prompt.clone(),
        None => base_brief(
            &store,
            name,
            &parent,
            profile,
            parent_profile,
            how.why.as_deref(),
        ),
    };
    let reserved = names::reserve(
        &store,
        &yard.root,
        options.name.as_deref(),
        how.stem.as_deref().unwrap_or(&parent.info.prompt),
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
            base: candidate
                .as_ref()
                .map_or(parent.info.base.clone(), |c| c.commit.clone()),
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
            seed: None,
            actor: options.actor.clone().or(parent.actor.clone()),
            task: Some(crate::tasks::joining_fork(
                yard,
                name,
                None,
                &parent.info.prompt,
                options,
            )?),
        },
    )
    .inspect_err(|_| store.release(&reserved))?;
    let (record, lease) = record;
    let new_name = record.info.name.clone();
    for activity in how.events {
        let event = crate::RecordedEvent {
            at_ms: now_ms(),
            activity,
        };
        if let Err(error) = store.append(&new_name, &event, Some(lease.fence())) {
            abandon(lease, record, &error);
            return Err(error);
        }
    }
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
            sandbox: Default::default(),
        },
        lease,
    )
}

/// The first prompt for a branch failed over from one that ran turns but
/// left no candidate: the original task, what it last said, and why.
fn base_brief(
    store: &crate::state::Store,
    name: &str,
    parent: &Record,
    profile: &Profile,
    parent_profile: &Profile,
    why: Option<&str>,
) -> String {
    let events = record::read(store, name).unwrap_or_default();
    let last = delegation::last_message(&events);
    let mut brief = format!(
        "{name} is being restarted with {}: do its task in a fresh session.\n\n\
         ## Original task\n{}\n\n\
         ## Progress so far\n{} turn(s) with {}, which left no changes.\n",
        profile.id, parent.info.prompt, parent.info.turns, parent_profile.id,
    );
    if !last.is_empty() {
        brief.push_str(&format!("\n## Its last message\n{last}\n"));
    }
    if let Some(why) = why {
        brief.push_str(&format!("\n## Why it was restarted\n{why}\n"));
    }
    brief.push_str("\nStart from the repository as it is and do the task.");
    brief
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
