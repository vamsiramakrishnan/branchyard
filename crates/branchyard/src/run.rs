//! Preparing branches for a turn: `run`, `run_on`, `send` and `fork`.
//!
//! Everything that can be checked without creating anything is checked
//! first: the harness, its executable, the base and the name.

use std::path::PathBuf;

use branchyard_harness::profiles::{self, Profile};
use branchyard_harness::SessionMode;
use branchyard_workspace::Commit;

use crate::engine::{self, Turn};
use crate::state::{now_ms, Record};
use crate::{
    git, harness, names, Branch, BranchInfo, BranchStatus, Error, NativeSession, TaskOptions, Yard,
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
struct Launch {
    profile: &'static Profile,
    command: Vec<String>,
}

fn launch(id: Option<&str>, command: Option<&[String]>) -> Result<Launch, Error> {
    let profile = harness::select(id)?;
    let command = harness::command(profile, command);
    harness::check_available(id.unwrap_or(profile.harness), &command)?;
    Ok(Launch { profile, command })
}

fn resolve_base(yard: &Yard, rev: Option<&str>) -> Result<String, Error> {
    let rev = rev.unwrap_or("HEAD");
    yard.repo
        .resolve(rev)
        .map(|commit| commit.0)
        .map_err(|_| Error::Git(format!("{rev:?} does not name a commit")))
}

/// What a new branch starts from.
struct NewBranch<'a> {
    name: &'a str,
    prompt: &'a str,
    profile: &'static Profile,
    base: String,
    parent: Option<String>,
    check: Option<Vec<String>>,
    command: Option<Vec<String>>,
    home: Option<PathBuf>,
    cost_baseline: Option<f64>,
}

/// Write the record for a reserved name and create its worktree. A
/// worktree that cannot be created leaves the branch `Failed`.
fn create(yard: &Yard, new: NewBranch<'_>) -> Result<Record, Error> {
    let store = yard.store();
    let branch = names::validate(new.name)?;
    let created_ms = now_ms();
    let mut record = Record {
        info: BranchInfo {
            name: new.name.to_owned(),
            git_branch: branch.branch(),
            worktree: store.worktree(new.name),
            prompt: new.prompt.to_owned(),
            harness: new.profile.harness.to_owned(),
            profile: new.profile.id.to_owned(),
            session: None,
            parent: new.parent,
            base: new.base.clone(),
            candidate: None,
            status: BranchStatus::Running,
            turns: 0,
            cost_usd: None,
            created_at: created_ms / 1000,
        },
        created_ms,
        check: new.check,
        command: new.command,
        home: new.home,
        cost_baseline: new.cost_baseline,
    };
    let created = {
        let _lock = git::lock();
        yard.repo
            .create_branch(&branch, &Commit(new.base), &record.info.worktree)
    };
    match created {
        Ok(workspace) => record.info.worktree = workspace.path,
        Err(error) => {
            record.info.status = BranchStatus::Failed {
                reason: format!("could not create the worktree: {error}"),
            }
        }
    }
    if let Some(home) = &record.home {
        std::fs::create_dir_all(home)?;
    }
    store.write(&record)?;
    Ok(record)
}

fn isolated_home(yard: &Yard, options: &TaskOptions, name: &str) -> Option<PathBuf> {
    options.isolated.then(|| yard.store().home(name))
}

pub(crate) fn run(yard: &Yard, prompt: &str, options: &TaskOptions) -> Result<Branch, Error> {
    let launch = launch(options.harness.as_deref(), options.command.as_deref())?;
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
        },
    );
    let record = record.inspect_err(|_| store.release(&name))?;
    engine::execute(Turn {
        yard,
        record,
        profile: launch.profile,
        command: launch.command,
        mode: SessionMode::Fresh,
        prompt,
        options,
        fork_source: None,
    })
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
        .map(|id| launch(Some(id), options.command.as_deref()))
        .collect::<Result<Vec<_>, _>>()?;
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
            },
        );
        match record {
            Ok(record) => turns.push(Turn {
                yard,
                record,
                profile: launch.profile,
                command: launch.command,
                mode: SessionMode::Fresh,
                prompt,
                options,
                fork_source: None,
            }),
            Err(error) => {
                for name in &reserved[index..] {
                    store.release(name);
                }
                return Err(error);
            }
        }
    }
    let results: Vec<Result<Branch, Error>> = std::thread::scope(|scope| {
        let handles: Vec<_> = turns
            .into_iter()
            .map(|turn| scope.spawn(move || engine::execute(turn)))
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
    let store = yard.store();
    let mut record = store.read(name)?;
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
    if !profile.driver().capabilities().resume {
        return Err(Error::Unsupported(format!(
            "{} cannot resume a session",
            profile.id
        )));
    }
    let session = record
        .info
        .session
        .as_deref()
        .and_then(NativeSession::new)
        .ok_or_else(|| Error::Unsupported(format!("{name} has no harness session to resume")))?;
    if options.command.is_some() {
        record.command = options.command.clone();
    }
    let command = harness::command(profile, record.command.as_deref());
    harness::check_available(profile.harness, &command)?;
    if !record.info.worktree.is_dir() {
        return Err(Error::State(format!(
            "{name}'s worktree {} is missing",
            record.info.worktree.display()
        )));
    }
    if options.check.is_some() {
        record.check = options.check.clone();
    }
    record.info.status = BranchStatus::Running;
    store.write(&record)?;
    engine::execute(Turn {
        yard,
        record,
        profile,
        command,
        mode: SessionMode::Resume(session),
        prompt,
        options,
        fork_source: None,
    })
}

pub(crate) fn fork(
    yard: &Yard,
    name: &str,
    prompt: &str,
    fresh_session: bool,
    options: &TaskOptions,
) -> Result<Branch, Error> {
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
    let same = profile.id == parent_profile.id;
    let session = parent.info.session.as_deref().and_then(NativeSession::new);
    let refusal = if !same {
        Some(format!(
            "a {} conversation cannot be forked into {}",
            parent_profile.id, profile.id
        ))
    } else if !profile.driver().capabilities().fork {
        Some(format!("{} cannot fork a session", profile.id))
    } else if session.is_none() {
        Some(format!("{name} has no harness session to fork"))
    } else {
        None
    };
    let mode = match (refusal, session) {
        (None, Some(session)) => SessionMode::Fork(session),
        (Some(_), _) | (None, None) if fresh_session => SessionMode::Fresh,
        (Some(reason), _) => {
            return Err(Error::Unsupported(format!(
                "{reason}; fork with a fresh session to start one on its candidate"
            )))
        }
        (None, None) => unreachable!("a missing session is a refusal"),
    };
    let forking = matches!(mode, SessionMode::Fork(_));
    let command = match (&options.command, same) {
        (Some(command), _) => Some(command.clone()),
        (None, true) => parent.command.clone(),
        (None, false) => None,
    };
    let launch_command = harness::command(profile, command.as_deref());
    harness::check_available(
        options.harness.as_deref().unwrap_or(profile.harness),
        &launch_command,
    )?;
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
            base: candidate.commit,
            parent: Some(name.to_owned()),
            check: options.check.clone().or(parent.check.clone()),
            command,
            home,
            cost_baseline,
        },
    )
    .inspect_err(|_| store.release(&reserved))?;
    engine::execute(Turn {
        yard,
        record,
        profile,
        command: launch_command,
        mode,
        prompt,
        options,
        fork_source: forking.then(|| parent.info.worktree.clone()),
    })
}
