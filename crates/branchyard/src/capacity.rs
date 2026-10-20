//! Capacity: at most so many branches run each harness at once in a
//! repository (`[usage] max_running`, [`Yard::use_max_running`]), as many
//! as a subscription or an API key takes. A delegated child whose harness
//! is full waits (`waiting`, as for a prerequisite) and starts when a branch
//! on that harness settles ([`crate::graph`]). A child's own ancestors do
//! not count against it, so a parent waiting on its children never holds
//! the slot they need.
//!
//! A spawn that asks for the `auto` harness is placed by the router among
//! the harnesses its parent may delegate to that can run here, preferring
//! one with a free slot ([`crate::delegation`]).
//!
//! Each engine counts from the store, so the limit is kept between turns
//! started one after another; engines starting children at the same moment
//! may run one past it.

use std::collections::{BTreeMap, BTreeSet};

use crate::projection::lock;
use crate::state::{Record, Store};
use crate::{BranchStatus, Error, Yard};

/// The harness a spawn names to have the router place its child.
pub const AUTO: &str = "auto";

impl Yard {
    /// Run at most `limits[id]` branches at once on each harness or
    /// profile `id` in this repository: a delegated child past its
    /// harness's limit waits for a free slot. Replaces the limits set
    /// before; empty, there are none. [`Yard::configure`] sets them from
    /// `[usage] max_running`.
    pub fn use_max_running(&self, limits: BTreeMap<String, u32>) {
        *lock(&self.hub.capacity) = limits;
    }

    /// The limits [`Yard::use_max_running`] set.
    pub fn max_running(&self) -> BTreeMap<String, u32> {
        lock(&self.hub.capacity).clone()
    }
}

/// The limit on a branch of `harness` and `profile`, and the key it was set
/// under: the profile's, else the harness's.
fn limit(yard: &Yard, harness: &str, profile: &str) -> Option<(String, u32)> {
    let limits = lock(&yard.hub.capacity);
    [profile, harness]
        .into_iter()
        .find_map(|key| limits.get(key).map(|max| (key.to_owned(), *max)))
}

/// Whether `record` runs on the harness or profile `key`.
fn on(record: &Record, key: &str) -> bool {
    record.info.harness == key || record.info.profile == key
}

/// `name` and its ancestors.
fn lineage(store: &Store, name: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut next = Some(name.to_owned());
    while let Some(branch) = next {
        if !found.insert(branch.clone()) {
            break;
        }
        next = store
            .backend()
            .read(&branch)
            .ok()
            .flatten()
            .and_then(|record| record.info.parent);
    }
    found
}

/// Why a branch of `harness` and `profile` whose lineage is `lineage` would
/// not start now: its harness is full with branches outside its lineage.
fn full_for(
    yard: &Yard,
    harness: &str,
    profile: &str,
    lineage: &BTreeSet<String>,
) -> Result<Option<String>, Error> {
    let Some((key, max)) = limit(yard, harness, profile) else {
        return Ok(None);
    };
    let busy: Vec<String> = yard
        .store()
        .list()?
        .into_iter()
        .filter(|r| r.info.status == BranchStatus::Running && on(r, &key))
        .filter(|r| !lineage.contains(&r.info.name))
        .map(|r| r.info.name)
        .collect();
    Ok((busy.len() >= max as usize).then(|| {
        format!(
            "a free slot on {key}, which runs {} of its {max} at once ({}; [usage] max_running)",
            busy.len(),
            busy.join(", ")
        )
    }))
}

/// Why `record`, waiting, does not start now: its harness is full. `None`
/// when it has a free slot or no limit.
pub(crate) fn full(yard: &Yard, record: &Record) -> Result<Option<String>, Error> {
    let store = yard.store();
    full_for(
        yard,
        &record.info.harness,
        &record.info.profile,
        &lineage(&store, &record.info.name),
    )
}

/// Why a new child of `parent` on the harness or profile `id` would wait
/// for a free slot. `None` when it would start, or `id` is unknown.
pub(crate) fn full_for_child(yard: &Yard, id: &str, parent: &str) -> Option<String> {
    let profile = crate::harness::select(Some(id)).ok()?;
    let store = yard.store();
    full_for(yard, profile.harness, profile.id, &lineage(&store, parent))
        .ok()
        .flatten()
}

/// The branches waiting that may start now that `name` settled: those on
/// its harness's limit, oldest first. None when its harness has no limit.
pub(crate) fn queued_behind(yard: &Yard, name: &str) -> Vec<Record> {
    let store = yard.store();
    let Ok(Some(settled)) = store.backend().read(name) else {
        return Vec::new();
    };
    let Some((key, _)) = limit(yard, &settled.info.harness, &settled.info.profile) else {
        return Vec::new();
    };
    let mut waiting: Vec<Record> = store
        .list()
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.info.status == BranchStatus::Waiting && on(r, &key))
        .collect();
    waiting.sort_by(|a, b| (a.created_ms, &a.info.name).cmp(&(b.created_ms, &b.info.name)));
    waiting
}
