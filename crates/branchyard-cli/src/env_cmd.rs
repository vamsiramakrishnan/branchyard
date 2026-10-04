//! `by env list|show|rebuild|prune`: the prepared environments of a
//! repository whose `[workspace]` has `prepare = true`. See
//! docs/environments.md. `by env pool status|fill|drain`: its warm pool
//! (`[workspace.pool]`). See docs/pools.md.
//!
//! `rebuild` and `pool fill` run the repository's setup (a fill does when
//! the environment is not built), so they need the same trust as a new
//! branch (`by workspace trust`), and a harness on a branch can never run
//! them. The other actions read or delete files under `.branchyard/` and
//! run nothing.
//!
//! The CLI never keeps a pool filled in the background: `by run` only
//! claims a ready slot. `by serve` and `by worker` refill after each claim;
//! locally, `by env pool fill` fills once.

use std::time::Duration;

use branchyard::{EnvironmentInfo, EnvironmentState, WorkspaceSpec, Yard};
use serde_json::json;

use crate::args::{EnvAction, PoolAction};
use crate::commands::{self, print, Env, Failure, Outcome, Target};
use crate::workspace_cmd;

/// `by env ACTION`.
pub fn main(env: &Env, target: &Target, action: &EnvAction, json: bool) -> Outcome {
    if let Target::Remote(_) = target {
        return Err(Failure::Message(
            "by env acts on a local repository's .branchyard/environments; a server's are on \
             its host"
                .into(),
        ));
    }
    let yard = commands::open()?;
    match action {
        EnvAction::List => list(&yard, json),
        EnvAction::Show { key } => show(&yard, key.as_deref(), json),
        EnvAction::Rebuild => rebuild(env, &yard, json),
        EnvAction::Prune {
            keys,
            keep,
            older_than,
        } => prune(&yard, keys, *keep, *older_than, json),
        EnvAction::Pool(action) => pool(env, &yard, action, json),
    }
}

/// The `[workspace]` that applies here, when it has a pool.
fn pool_spec(yard: &Yard) -> Result<(WorkspaceSpec, workspace_cmd::Resolved), Failure> {
    let resolved = workspace_cmd::resolve(yard.root())?;
    match resolved {
        Some(resolved) if resolved.workspace.pool.is_some() => Ok((resolved.spec(), resolved)),
        _ => Err(Failure::Message(
            "[workspace] has no pool here: add [workspace.pool] with a size (see docs/pools.md)"
                .into(),
        )),
    }
}

fn slot_line(slot: &branchyard::PoolSlot) -> String {
    let state = match slot.state {
        branchyard::PoolSlotState::Filling => "filling",
        branchyard::PoolSlotState::Ready => "ready",
        branchyard::PoolSlotState::Claimed => "claimed",
    };
    let short = |s: &str| s.chars().take(12).collect::<String>();
    let mut line = format!(
        "{:<20} {state:<8} {:<13} {:<13} {:<9}",
        slot.id,
        short(&slot.base),
        slot.environment
            .as_deref()
            .map_or_else(|| "-".into(), short),
        age(slot.changed_ms),
    );
    if let Some(ms) = slot.fill_ms {
        line.push_str(&format!(" made in {:.1}s", ms as f64 / 1000.0));
    }
    if let Some(branch) = &slot.branch {
        line.push_str(&format!(" for {branch}"));
    }
    line.push('\n');
    line
}

fn pool(env: &Env, yard: &Yard, action: &PoolAction, json: bool) -> Outcome {
    match action {
        PoolAction::Status => {
            let (spec, _) = pool_spec(yard)?;
            let status = yard.pool_status(&spec)?;
            if json {
                return print(&to_json(&serde_json::to_value(&status).unwrap_or_default()));
            }
            let mut text = format!(
                "pool {} at {}: {} of {} ready\n",
                &status.recipe[..status.recipe.len().min(12)],
                status
                    .base
                    .as_deref()
                    .map_or("?", |b| &b[..b.len().min(12)]),
                status.ready(),
                status.size
            );
            if !status.slots.is_empty() {
                text.push_str(
                    "SLOT                 STATE    BASE          ENVIRONMENT   CHANGED\n",
                );
                for slot in &status.slots {
                    text.push_str(&slot_line(slot));
                }
            }
            if status.other > 0 {
                text.push_str(&format!(
                    "{} slot(s) of an earlier setup: the next fill discards them\n",
                    status.other
                ));
            }
            print(&text)
        }
        PoolAction::Fill => {
            if workspace_cmd::in_harness().is_some() {
                return Err(Failure::Message(
                    "a harness running on a branch cannot fill the pool: it may run the \
                     repository's setup, which is a person's decision"
                        .into(),
                ));
            }
            let (spec, resolved) = pool_spec(yard)?;
            workspace_cmd::require_trust(env, &resolved)?;
            let filled = yard.fill_pool(&spec)?;
            if json {
                print(&to_json(&serde_json::to_value(&filled).unwrap_or_default()))?;
            } else {
                let mut text = String::new();
                for (id, why) in filled.reclaimed.iter().chain(&filled.discarded) {
                    text.push_str(&format!("removed {id}: {why}\n"));
                }
                for slot in &filled.made {
                    text.push_str(&format!(
                        "made    {} in {:.1}s\n",
                        slot.id,
                        slot.fill_ms.unwrap_or(0) as f64 / 1000.0
                    ));
                }
                if let Some(why) = &filled.skipped {
                    text.push_str(&format!("{why}\n"));
                }
                text.push_str(&format!("{} of {} ready\n", filled.ready, filled.size));
                if let Some(error) = &filled.error {
                    text.push_str(&format!("stopped: {error}\n"));
                }
                print(&text)?;
            }
            match filled.error {
                Some(_) => Err(Failure::Reported),
                None => Ok(()),
            }
        }
        PoolAction::Drain => {
            let drained = yard.drain_pool()?;
            if json {
                return print(&to_json(
                    &serde_json::to_value(&drained).unwrap_or_default(),
                ));
            }
            let mut text = String::new();
            for (id, why) in &drained.removed {
                text.push_str(&format!("removed {id}: {why}\n"));
            }
            for (id, why) in &drained.kept {
                text.push_str(&format!("kept    {id}: {why}\n"));
            }
            if text.is_empty() {
                text.push_str("nothing to drain\n");
            }
            print(&text)
        }
    }
}

/// The `[workspace]` that applies here, as the engine stores it with a new
/// branch, when it prepares environments.
fn spec(yard: &Yard) -> Result<Option<(WorkspaceSpec, workspace_cmd::Resolved)>, Failure> {
    let Some(resolved) = workspace_cmd::resolve(yard.root())? else {
        return Ok(None);
    };
    let spec = resolved.spec();
    Ok(spec.prepare.then_some((spec, resolved)))
}

fn age(ms: u64) -> String {
    let now = branchyard_support::time::now_ms();
    let seconds = now.saturating_sub(ms) / 1000;
    match seconds {
        s if s < 120 => format!("{s}s ago"),
        s if s < 7200 => format!("{}m ago", s / 60),
        s if s < 172_800 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86400),
    }
}

fn to_json(value: &serde_json::Value) -> String {
    format!(
        "{}\n",
        serde_json::to_string_pretty(value).unwrap_or_default()
    )
}

fn list(yard: &Yard, json: bool) -> Outcome {
    let envs = yard.environments();
    let current = spec(yard)?.map(|(spec, _)| yard.environment_key(&spec));
    if json {
        return print(&to_json(&json!({
            "current": current,
            "environments": envs,
        })));
    }
    if envs.is_empty() {
        return print(match current {
            Some(_) => "No prepared environments yet: the next branch builds one.\n",
            None => {
                "No prepared environments. Set prepare = true in [workspace] (see \
                 docs/environments.md).\n"
            }
        });
    }
    let mut text =
        String::from("KEY           STATE   PLACE         BUILT BY            BUILT     USED\n");
    for info in &envs {
        let state = match info.state {
            EnvironmentState::Good => "good",
            EnvironmentState::Failed => "failed",
        };
        let mark = match current.as_deref() == Some(info.key.as_str()) {
            true => " (current)",
            false => "",
        };
        let place: String = info.place.chars().take(12).collect();
        text.push_str(&format!(
            "{:<13} {state:<7} {place:<13} {:<19} {:<9} {}{mark}\n",
            &info.key[..info.key.len().min(12)],
            info.built_by,
            age(info.built_ms),
            age(info.last_used_ms),
        ));
    }
    print(&text)
}

fn find(yard: &Yard, key: &str) -> Option<EnvironmentInfo> {
    let envs = yard.environments();
    // A good one before a failure of the same key.
    let mut matching: Vec<EnvironmentInfo> = envs
        .into_iter()
        .filter(|i| i.key.starts_with(key))
        .collect();
    matching.sort_by_key(|i| i.state != EnvironmentState::Good);
    matching.into_iter().next()
}

fn show(yard: &Yard, key: Option<&str>, json: bool) -> Outcome {
    let key =
        match key {
            Some(key) => key.to_owned(),
            None => match spec(yard)? {
                Some((spec, _)) => yard.environment_key(&spec),
                None => return Err(Failure::Message(
                    "[workspace] does not prepare environments here (prepare = true); name a key \
                     from `by env list`"
                        .into(),
                )),
            },
        };
    let Some(info) = find(yard, &key) else {
        if json {
            return print(&to_json(&json!({ "key": key, "environment": null })));
        }
        return print(&format!(
            "environment {key}: not built yet; the next branch with this key builds it, or run \
             `by env rebuild`\n"
        ));
    };
    if json {
        return print(&to_json(&serde_json::to_value(&info).unwrap_or_default()));
    }
    let mut text = format!(
        "key       {}\nrecipe    {}\nplace     {}\nstate     {}\nbuilt     {} by {}\nused      {}\n",
        info.key,
        info.recipe,
        info.place,
        match info.state {
            EnvironmentState::Good => "good",
            EnvironmentState::Failed => "failed",
        },
        age(info.built_ms),
        info.built_by,
        age(info.last_used_ms),
    );
    if let Some(reason) = &info.reason {
        text.push_str(&format!("reason    {reason}\n"));
    }
    for input in &info.inputs {
        text.push_str(&format!(
            "input     {} {}\n",
            input.path,
            &input.digest[..input.digest.len().min(12)]
        ));
    }
    for command in &info.setup {
        text.push_str(&format!("setup     {command}\n"));
    }
    if !info.produced.is_empty() {
        text.push_str(&format!("produced  {}\n", info.produced.join(", ")));
    }
    if !info.share.is_empty() {
        text.push_str(&format!("shared    {}\n", info.share.join(", ")));
    }
    if let Some(snapshot) = &info.snapshot {
        text.push_str(&format!(
            "snapshot  {} {} ({})\n",
            snapshot.provider,
            snapshot.handle,
            snapshot.method.describe()
        ));
    }
    print(&text)
}

fn rebuild(env: &Env, yard: &Yard, json: bool) -> Outcome {
    if workspace_cmd::in_harness().is_some() {
        return Err(Failure::Message(
            "a harness running on a branch cannot rebuild an environment: it runs the \
             repository's setup, which is a person's decision"
                .into(),
        ));
    }
    let Some((spec, resolved)) = spec(yard)? else {
        return Err(Failure::Message(
            "[workspace] does not prepare environments here: set prepare = true (see \
             docs/environments.md)"
                .into(),
        ));
    };
    workspace_cmd::require_trust(env, &resolved)?;
    let built = yard.rebuild_environment(&spec)?;
    if json {
        print(&to_json(&serde_json::to_value(&built).unwrap_or_default()))?;
    } else {
        match &built.environment {
            Some(info) => print(&format!(
                "built environment {} ({} produced: {})\n",
                &info.key[..info.key.len().min(12)],
                info.produced.len(),
                info.produced.join(", ")
            ))?,
            None => print(&format!(
                "environment {} failed to build: {}\nthe last good build is kept; output:\n{}",
                &built.key[..built.key.len().min(12)],
                built.report.failure(),
                built.report.output
            ))?,
        }
    }
    match built.environment {
        Some(_) => Ok(()),
        None => Err(Failure::Reported),
    }
}

fn prune(
    yard: &Yard,
    keys: &[String],
    keep: Option<usize>,
    older_than: Option<u64>,
    json: bool,
) -> Outcome {
    let only: Vec<String> = keys
        .iter()
        .map(|k| find(yard, k).map_or_else(|| k.clone(), |i| i.key))
        .collect();
    let pruned = yard.prune_environments(
        keep.unwrap_or(branchyard::ENVIRONMENT_DEFAULT_KEEP),
        older_than.map_or(branchyard::ENVIRONMENT_DEFAULT_MAX_AGE, |days| {
            Duration::from_secs(days * 86400)
        }),
        &only,
    );
    if json {
        return print(&to_json(&serde_json::to_value(&pruned).unwrap_or_default()));
    }
    let mut text = String::new();
    for (key, why) in &pruned.removed {
        text.push_str(&format!("removed {key}: {why}\n"));
    }
    for (key, why) in &pruned.kept {
        text.push_str(&format!("kept    {key}: {why}\n"));
    }
    if text.is_empty() {
        text.push_str("nothing to prune\n");
    }
    print(&text)
}
