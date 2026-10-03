//! `by sync [TASK]`, `by sync status|pull|ls|gc|scrub|hold|rm|rotate-key`:
//! this repository's branches synced to the remote `[sync]` names in the
//! user configuration, through the outbox in `.branchyard/sync.db`. See
//! docs/sync.md.

use std::sync::Arc;

use branchyard::services::Clock;
use branchyard_sync::outbox::Outbox;
use branchyard_sync::replicator::{describe, Replicator};
use branchyard_sync::seal::Encryption;
use branchyard_sync::yard::YardTasks;
use branchyard_sync::{Remote, SyncConfig};
use serde_json::json;

use crate::args::{Globals, SyncAction};
use crate::commands::{self, print, Failure, Outcome};
use crate::json;

/// The variable holding the new passphrase for `rotate-key --to passphrase`.
const ENV_NEW_PASSPHRASE: &str = "BRANCHYARD_SYNC_NEW_PASSPHRASE";

fn failure(e: branchyard_sync::Error) -> Failure {
    Failure::Message(format!("sync: {e}"))
}

/// `[sync]` from the user configuration.
fn config() -> Result<SyncConfig, Failure> {
    let cwd = std::env::current_dir()?;
    let env = |name: &str| std::env::var(name).ok();
    let effective =
        crate::setup_io::load(&cwd, Some(&env)).map_err(|e| Failure::Message(e.to_string()))?;
    let s = effective.config.sync;
    let Some(remote) = s.remote else {
        return Err(Failure::Message(
            "no [sync] remote: add `[sync]` with `remote = \"gs://bucket/prefix\"` (or s3://, \
             az://, file:///, git+https://) to ~/.config/branchyard/config.toml; see docs/sync.md"
                .into(),
        ));
    };
    let config = SyncConfig {
        remote,
        encrypt: s.encrypt,
        passphrase_file: s.passphrase_file.map(Into::into),
        algorithm: s.algorithm,
        interval: s.interval,
        bandwidth: s.bandwidth,
        concurrency: s.concurrency.map(|c| c as usize),
        retention: s.retention,
        grace: s.grace,
        quota: s.quota,
        device: s.device,
    };
    config.check().map_err(failure)?;
    Ok(config)
}

pub fn main(
    globals: &Globals,
    task: Option<&str>,
    action: Option<&SyncAction>,
    as_json: bool,
) -> Outcome {
    if globals.remote.is_some() {
        return Err(Failure::Message(
            "by sync runs on this machine's repository; a server syncs with the `sync` of its own \
             configuration (docs/sync.md). Run it without --remote"
                .into(),
        ));
    }
    let config = config()?;
    let yard = commands::open()?;
    let outbox = Arc::new(Outbox::open(&Outbox::path_for(yard.root())).map_err(failure)?);
    let remote = Arc::new(config.open(&outbox, Clock::system()).map_err(failure)?);
    let tasks = YardTasks::new(yard.clone()).map_err(failure)?;
    let replicator = Replicator::new(remote.clone(), outbox, Arc::new(tasks.clone()));
    let resolve = |t: &str| tasks.resolve(t).map_err(failure);
    match action {
        None => sync(
            &replicator,
            task.map(resolve).transpose()?.as_deref(),
            as_json,
        ),
        Some(SyncAction::Status) => status(&replicator, as_json),
        Some(SyncAction::Pull { task }) => {
            let id = resolve(task)?;
            let report = replicator.pull(&id).map_err(failure)?;
            if as_json {
                return print(&json::text(&json!({ "task": id, "report": report })));
            }
            print(&format!("{id}: {}\n", describe(&report)))
        }
        Some(SyncAction::Ls) => {
            let listed = remote.tasks().map_err(failure)?;
            if as_json {
                return print(&json::text(
                    &json!({ "remote": remote.url(), "tasks": listed }),
                ));
            }
            let mut out = format!("{}\n", remote.url());
            if listed.is_empty() {
                out.push_str("no tasks\n");
            }
            for t in listed {
                out.push_str(&format!(
                    "{}  seq {}  {} refs  {} bytes  by {}{}\n",
                    t.task,
                    t.seq,
                    t.refs.len(),
                    t.bytes,
                    t.device,
                    if t.held { "  held" } else { "" }
                ));
            }
            print(&out)
        }
        Some(SyncAction::Gc { dry_run }) => {
            let report = remote.gc(*dry_run).map_err(failure)?;
            if as_json {
                return print(&json::text(&json!(report)));
            }
            let mut out = String::new();
            if let Some(why) = &report.deferred {
                out.push_str(&format!("deferred: {why}\n"));
            }
            out.push_str(&format!(
                "{} manifests, {} referenced, {} unreferenced ({} within the grace period)\n",
                report.manifests, report.referenced, report.unreferenced, report.in_grace
            ));
            for task in &report.expired_tasks {
                out.push_str(&format!("expired {task}\n"));
            }
            let verb = if *dry_run { "would delete" } else { "deleted" };
            out.push_str(&format!(
                "{verb} {} objects, {} bytes; {} bytes in {} objects remain\n",
                report.deleted.len(),
                report.bytes_freed,
                report.usage.bytes,
                report.usage.objects
            ));
            print(&out)
        }
        Some(SyncAction::Scrub { sample, seed }) => {
            let dirs: Vec<std::path::PathBuf> = Vec::new();
            let report = remote
                .scrub(
                    *sample,
                    seed.unwrap_or_else(branchyard_sync::util::random_seed),
                    &dirs,
                )
                .map_err(failure)?;
            if as_json {
                print(&json::text(&json!(report)))?;
            } else {
                let mut out = format!(
                    "checked {} of {} objects and {} manifests\n",
                    report.checked, report.objects, report.manifests
                );
                for (key, why) in &report.corrupt {
                    out.push_str(&format!("corrupt {key}: {why}\n"));
                }
                for key in &report.missing {
                    out.push_str(&format!("missing {key}\n"));
                }
                for key in &report.repaired {
                    out.push_str(&format!("repaired {key}\n"));
                }
                print(&out)?;
            }
            match report.clean() {
                true => Ok(()),
                false => Err(Failure::Message("sync: scrub found damage".into())),
            }
        }
        Some(SyncAction::Hold {
            task,
            reason,
            release,
        }) => {
            let id = resolve(task)?;
            if *release {
                let released = remote.release_hold(&id).map_err(failure)?;
                if as_json {
                    return print(&json::text(&json!({ "task": id, "released": released })));
                }
                return print(&match released {
                    true => format!("released the hold on {id}\n"),
                    false => format!("{id} was not on hold\n"),
                });
            }
            let by = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
            remote
                .hold(&id, reason.as_deref().unwrap_or(""), &by)
                .map_err(failure)?;
            if as_json {
                return print(&json::text(&json!({ "task": id, "held": true })));
            }
            print(&format!("{id} is on legal hold\n"))
        }
        Some(SyncAction::Rm { task }) => {
            let id = resolve(task)?;
            let removed = remote.remove_task(&id).map_err(failure)?;
            if as_json {
                return print(&json::text(&json!({ "task": id, "removed": removed })));
            }
            print(&match removed {
                true => {
                    format!("removed {id} from the remote; `by sync gc` reclaims its objects\n")
                }
                false => format!("the remote has no task {id}\n"),
            })
        }
        Some(SyncAction::RotateKey { to }) => rotate(&config, &remote, to.as_deref(), as_json),
    }
}

fn sync(replicator: &Replicator, task: Option<&str>, as_json: bool) -> Outcome {
    let (synced, failed) = match task {
        Some(id) => match replicator.sync_task(id) {
            Ok(report) => (vec![report], vec![]),
            Err(e) => (vec![], vec![(id.to_owned(), e.to_string())]),
        },
        None => {
            replicator.enqueue_all().map_err(failure)?;
            let drained = replicator.drain().map_err(failure)?;
            (drained.synced, drained.failed)
        }
    };
    if as_json {
        print(&json::text(&json!({
            "remote": replicator.remote().url(),
            "synced": synced,
            "failed": failed.iter().map(|(t, e)| json!({"task": t, "error": e})).collect::<Vec<_>>(),
        })))?;
    } else {
        let mut out = String::new();
        if synced.is_empty() && failed.is_empty() {
            out.push_str("no branches to sync\n");
        }
        for report in &synced {
            out.push_str(&format!("{}: {}\n", report.task, describe(report)));
        }
        for (task, error) in &failed {
            out.push_str(&format!("{task}: failed: {error} (queued to try again)\n"));
        }
        print(&out)?;
    }
    match failed.is_empty() {
        true => Ok(()),
        false => Err(Failure::Message(format!(
            "sync: {} of {} tasks failed",
            failed.len(),
            failed.len() + synced.len()
        ))),
    }
}

fn status(replicator: &Replicator, as_json: bool) -> Outcome {
    let status = replicator.status().map_err(failure)?;
    if as_json {
        return print(&json::text(&json!(status)));
    }
    let mut out = format!(
        "remote    {}\ndevice    {}\nencrypted {}\n",
        status.remote,
        status.device,
        if status.encrypted { "yes" } else { "no" }
    );
    if status.tasks.is_empty() {
        out.push_str("no branches\n");
    }
    for t in &status.tasks {
        out.push_str(&format!(
            "{}  {}  seq {}  {} refs{}{}\n",
            t.task,
            t.state,
            t.seq,
            t.refs,
            match t.lag_ms {
                0 => String::new(),
                ms => format!("  lag {}s", ms / 1000),
            },
            t.last_error
                .as_deref()
                .map(|e| format!("  error: {e}"))
                .unwrap_or_default()
        ));
    }
    let c = &status.counters;
    out.push_str(&format!(
        "sent {} bytes in {} objects, received {} bytes in {} objects; {} swaps, {} swap \
         conflicts, {} divergences, {} retries, {} refused as corrupt\n",
        c.bytes_up,
        c.objects_up,
        c.bytes_down,
        c.objects_down,
        c.swaps,
        c.swap_conflicts,
        c.divergences,
        c.retries,
        c.corrupt
    ));
    print(&out)
}

fn rotate(config: &SyncConfig, remote: &Remote, to: Option<&str>, as_json: bool) -> Outcome {
    let Encryption::Envelope { wrapper, .. } = config.encryption().map_err(failure)? else {
        return Err(Failure::Message(
            "sync: this remote is not encrypted; there is no key to rotate".into(),
        ));
    };
    let new: Option<Box<dyn branchyard_sync::kms::Wrapper>> = match to {
        None => None,
        Some("passphrase") => {
            let passphrase = std::env::var(ENV_NEW_PASSPHRASE).map_err(|_| {
                Failure::Message(format!(
                    "sync: --to passphrase reads the new passphrase from {ENV_NEW_PASSPHRASE}"
                ))
            })?;
            Some(Box::new(
                branchyard_sync::kms::Passphrase::new(&passphrase).map_err(failure)?,
            ))
        }
        Some(url) => Some(branchyard_sync::kms::from_url(url).map_err(failure)?),
    };
    let report = remote
        .rotate_key(wrapper.as_ref(), new.as_deref())
        .map_err(failure)?;
    if as_json {
        return print(&json::text(&json!(report)));
    }
    print(&format!(
        "key version {}: {} objects rewrapped, {} already current; retired versions {:?}\n{}",
        report.new_version,
        report.rewrapped,
        report.already_current,
        report.retired,
        match to {
            Some(_) => "update [sync] encrypt (and the passphrase you set) to the new wrapper\n",
            None => "",
        }
    ))
}
