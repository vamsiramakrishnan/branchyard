//! `by env list|show|rebuild|prune`: the prepared environments of a
//! repository whose `[workspace]` has `prepare = true`. See
//! docs/environments.md.
//!
//! `rebuild` runs the repository's setup, so it needs the same trust as a
//! new branch (`by workspace trust`), and a harness on a branch can never
//! run it. The other actions read or delete files under
//! `.branchyard/environments/` and run nothing.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use branchyard::{EnvironmentInfo, EnvironmentState, WorkspaceSpec, Yard};
use serde_json::json;

use crate::args::EnvAction;
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
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
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
        .map(|k| find(yard, k).map(|i| i.key).unwrap_or_else(|| k.clone()))
        .collect();
    let pruned = yard.prune_environments(
        keep.unwrap_or(branchyard::ENVIRONMENT_DEFAULT_KEEP),
        older_than
            .map(|days| Duration::from_secs(days * 86400))
            .unwrap_or(branchyard::ENVIRONMENT_DEFAULT_MAX_AGE),
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
