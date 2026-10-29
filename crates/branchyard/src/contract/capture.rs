//! Bounded descriptor-based outbox reads and producer-fenced publication.

use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path};

use rustix::fs::{open, openat, Mode, OFlags};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::ResultCapture;
use crate::state::{Fence, Record};
use crate::storage::{ArtifactRef, Lineage, NewArtifact};
use crate::{BranchStatus, Error, Yard};

/// One fixed receipt per producer incarnation and engine turn.
pub(crate) const CAPTURE_STEP: &str = "capture_artifacts";
const MANIFEST_MAX: u64 = 1024 * 1024;

pub(super) fn default_max_files() -> u32 {
    32
}
pub(super) fn default_max_bytes() -> u64 {
    256 * 1024 * 1024
}

fn relative(path: &str) -> Result<Vec<&std::ffi::OsStr>, Error> {
    let parts: Vec<_> = Path::new(path).components().collect();
    if parts.is_empty()
        || parts
            .iter()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(Error::Denied(format!(
            "artifact capture needs a relative path without traversal: {path}"
        )));
    }
    Ok(parts
        .into_iter()
        .filter_map(|part| match part {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect())
}

pub(super) fn check_config(capture: &ResultCapture) -> Result<(), Error> {
    relative(&capture.manifest)?;
    if capture.max_files == 0 || capture.max_files > 1024 || capture.max_bytes == 0 {
        return Err(Error::Denied(
            "artifact capture needs 1..=1024 files and a positive byte limit".into(),
        ));
    }
    Ok(())
}

/// Walk using pinned directory descriptors, refusing symlinks at every component.
fn descend(root: &File, parts: &[&std::ffi::OsStr]) -> Result<File, Error> {
    let mut directory = root
        .try_clone()
        .map_err(|error| Error::State(format!("clone capture directory: {error}")))?;
    for part in parts {
        directory = File::from(
            openat(
                &directory,
                *part,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|error| {
                Error::Denied(format!(
                    "open capture directory {}: {error}",
                    part.to_string_lossy()
                ))
            })?,
        );
    }
    Ok(directory)
}

fn regular(directory: &File, name: &std::ffi::OsStr) -> Result<File, Error> {
    let file = File::from(
        openat(
            directory,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| {
            Error::Denied(format!(
                "open captured file {}: {error}",
                name.to_string_lossy()
            ))
        })?,
    );
    let metadata = file
        .metadata()
        .map_err(|error| Error::State(format!("stat captured file: {error}")))?;
    if !metadata.is_file() {
        return Err(Error::Denied(
            "captured input must be a regular file".into(),
        ));
    }
    Ok(file)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    files: Vec<Entry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    path: String,
    name: Option<String>,
    media_type: String,
    sha256: String,
}

fn sha256(text: &str) -> bool {
    text.len() == 64 && text == text.to_ascii_lowercase() && hex::decode(text).is_ok()
}

pub(crate) fn validate_capture_batch(
    fence: &Fence,
    manifest_digest: &str,
    artifacts: &[NewArtifact],
) -> Result<(), Error> {
    if !sha256(manifest_digest)
        || artifacts.iter().any(|new| {
            new.publisher_branch != fence.branch || new.publisher_incarnation != fence.incarnation
        })
    {
        return Err(Error::Denied(
            "capture publications must belong to their fenced producer".into(),
        ));
    }
    Ok(())
}

/// Collect only completed producer work, after the provider returned its files.
pub(crate) fn collect_artifacts(
    yard: &Yard,
    record: &Record,
    fence: &Fence,
) -> Result<Vec<ArtifactRef>, Error> {
    let Some(capture) = record
        .result_contract
        .as_ref()
        .and_then(|contract| contract.capture.as_ref())
    else {
        return Ok(Vec::new());
    };
    if !matches!(
        record.info.status,
        BranchStatus::Ready | BranchStatus::NoChanges | BranchStatus::Merged { .. }
    ) {
        return Ok(Vec::new());
    }
    check_config(capture)?;
    let parts = relative(&capture.manifest)?;
    let (manifest_name, parents) = parts
        .split_last()
        .ok_or_else(|| Error::Denied("empty capture manifest path".into()))?;
    let root = File::from(
        open(
            &record.info.worktree,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| Error::Denied(format!("open producer worktree: {error}")))?,
    );
    let directory = descend(&root, parents)?;
    let mut bytes = Vec::new();
    regular(&directory, manifest_name)?
        .take(MANIFEST_MAX + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| Error::State(format!("read capture manifest: {error}")))?;
    if bytes.len() as u64 > MANIFEST_MAX {
        return Err(Error::Denied("capture manifest exceeds 1 MiB".into()));
    }
    let manifest_digest = hex::encode(Sha256::digest(&bytes));
    let store = yard.store();
    if store
        .backend()
        .steps(&fence.branch, fence.turn)?
        .iter()
        .any(|step| step.step == CAPTURE_STEP)
    {
        return store
            .storage()
            .capture_artifacts(fence, &manifest_digest, &[]);
    }
    let manifest: Manifest = serde_json::from_slice(&bytes)
        .map_err(|error| Error::Denied(format!("invalid artifact capture manifest: {error}")))?;
    if manifest.schema_version != 1
        || manifest.files.is_empty()
        || manifest.files.len() > capture.max_files as usize
    {
        return Err(Error::Denied(
            "capture manifest needs schema_version 1 and files within max_files".into(),
        ));
    }
    let lineage = Lineage::load(&store)?;
    if lineage.id(&fence.branch)? != fence.incarnation {
        return Err(Error::Fenced("capture producer was replaced".into()));
    }
    let mut seen = HashSet::new();
    let mut remaining = capture.max_bytes;
    let mut artifacts = Vec::new();
    for entry in manifest.files {
        let parts = relative(&entry.path)?;
        if !seen.insert(parts.iter().copied().collect::<std::path::PathBuf>())
            || (parts.len() == 1 && parts[0] == *manifest_name)
        {
            return Err(Error::Denied(
                "capture manifest contains a duplicate or itself".into(),
            ));
        }
        if !sha256(&entry.sha256)
            || entry.media_type.is_empty()
            || entry.media_type.len() > 255
            || entry.media_type.chars().any(char::is_control)
        {
            return Err(Error::Denied(
                "captured file needs lowercase SHA-256 and a media type".into(),
            ));
        }
        let (name, parents) = parts
            .split_last()
            .ok_or_else(|| Error::Denied("empty captured path".into()))?;
        let mut file = regular(&descend(&directory, parents)?, name)?;
        let (digest, size) = crate::storage::store_open_blob(
            store.dir(),
            &mut file,
            remaining,
            Some(&entry.sha256),
        )?;
        remaining -= size;
        let name = entry
            .name
            .unwrap_or_else(|| name.to_string_lossy().into_owned());
        if name.trim().is_empty() || name.len() > 255 || name.chars().any(char::is_control) {
            return Err(Error::Denied(
                "captured display name is empty or invalid".into(),
            ));
        }
        artifacts.push(NewArtifact {
            digest,
            size,
            name,
            media_type: entry.media_type,
            publisher_branch: fence.branch.clone(),
            turn: u64::from(record.info.turns),
            publisher_incarnation: fence.incarnation,
            ancestry: crate::storage::ancestry_of(&store, &fence.branch),
            ancestry_incarnations: lineage.ancestors(fence.incarnation),
            labels: BTreeMap::from([
                ("capture.manifest_sha256".into(), manifest_digest.clone()),
                ("capture.path".into(), entry.path),
                ("capture.manifest".into(), capture.manifest.clone()),
                ("capture.engine_turn".into(), fence.turn.to_string()),
            ]),
        });
    }
    store
        .storage()
        .capture_artifacts(fence, &manifest_digest, &artifacts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn pinned_directories_do_not_follow_a_replaced_path_or_a_leaf_symlink() {
        let scratch = branchyard_testkit::Scratch::new("capture-descriptors");
        let root = scratch.path();
        std::fs::create_dir(root.join("safe")).unwrap();
        std::fs::create_dir(root.join("outside")).unwrap();
        std::fs::write(root.join("safe/evidence"), b"original").unwrap();
        std::fs::write(root.join("outside/evidence"), b"outside").unwrap();
        let root = File::open(root).unwrap();
        let held = descend(&root, &[OsStr::new("safe")]).unwrap();
        std::fs::rename(scratch.path().join("safe"), scratch.path().join("pinned")).unwrap();
        std::os::unix::fs::symlink(scratch.path().join("outside"), scratch.path().join("safe"))
            .unwrap();
        assert!(descend(&root, &[OsStr::new("safe")]).is_err());
        let mut original = String::new();
        regular(&held, OsStr::new("evidence"))
            .unwrap()
            .read_to_string(&mut original)
            .unwrap();
        assert_eq!(original, "original");
        std::os::unix::fs::symlink("evidence", scratch.path().join("pinned/alias")).unwrap();
        assert!(regular(&held, OsStr::new("alias")).is_err());
        assert!(regular(&root, OsStr::new("outside")).is_err());
    }

    #[test]
    fn malformed_manifest_contracts_and_capture_limits_are_refused() {
        for value in [
            serde_json::json!({"schema_version":1,"files":[],"actor":"other"}),
            serde_json::json!({"schema_version":true,"files":[]}),
            serde_json::json!({"schema_version":1,"files":[{"path":"x","sha256":"hash","media_type":"text/plain","grant":"*"}]}),
        ] {
            assert!(serde_json::from_value::<Manifest>(value).is_err());
        }
        let mut capture = ResultCapture::new("outputs/artifacts.json");
        capture.max_files = 1025;
        assert!(check_config(&capture).is_err());
        capture.max_files = 1;
        capture.max_bytes = 0;
        assert!(check_config(&capture).is_err());
        assert!(!sha256(&"A".repeat(64)));
        assert!(!sha256(&"x".repeat(64)));
    }

    #[test]
    fn a_completed_capture_reuses_receipt_without_reopening_outputs() {
        let repo = branchyard_testkit::Repo::init("unused-by");
        let yard = Yard::open(&repo.root).unwrap();
        let store = yard.store();
        let mut record: Record = serde_json::from_value(serde_json::json!({
            "info": {"name":"producer", "git_branch":"by/producer", "worktree":repo.root,
                "prompt":"produce", "harness":"fake", "profile":"fake", "session":null,
                "parent":null, "base":"base", "candidate":null, "status":{"state":"no_changes"},
                "turns":1, "cost_usd":null, "created_at":0},
            "created_ms":0, "check":null, "command":null, "home":null, "cost_baseline":null
        }))
        .unwrap();
        let mut contract =
            super::super::ResultContract::deliverable(serde_json::json!({"type":"object"}))
                .unwrap();
        contract.capture = Some(ResultCapture::new("outputs/artifacts.json"));
        record.result_contract = Some(contract);
        store.backend().write(&record, None).unwrap();
        let crate::state::Acquired::Granted(first_attempt) = store
            .backend()
            .acquire(&record, store.owner(), crate::state::LEASE_TTL)
            .unwrap()
        else {
            panic!("grant");
        };
        store
            .backend()
            .finish(&first_attempt, Some(&record), None)
            .unwrap();
        let crate::state::Acquired::Granted(fence) = store
            .backend()
            .acquire(&record, store.owner(), crate::state::LEASE_TTL)
            .unwrap()
        else {
            panic!("grant");
        };
        assert_eq!(fence.turn, 2);
        std::fs::create_dir(repo.root.join("outputs")).unwrap();
        std::fs::write(repo.root.join("outputs/data.txt"), b"payload").unwrap();
        let manifest = serde_json::json!({"schema_version":1,"files":[{"path":"data.txt",
            "media_type":"text/plain","sha256":hex::encode(Sha256::digest(b"payload"))}]})
        .to_string();
        std::fs::write(repo.root.join("outputs/artifacts.json"), &manifest).unwrap();
        let first = collect_artifacts(&yard, &record, &fence).unwrap();
        assert_eq!(
            first[0].turn, 1,
            "artifact turns keep the submitted-turn count"
        );
        assert_eq!(first[0].labels["capture.engine_turn"], "2");
        std::fs::remove_file(repo.root.join("outputs/data.txt")).unwrap();
        assert_eq!(collect_artifacts(&yard, &record, &fence).unwrap(), first);
        assert_eq!(yard.artifacts("producer").unwrap(), first);
        std::fs::write(repo.root.join("outputs/artifacts.json"), manifest + "\n").unwrap();
        assert!(collect_artifacts(&yard, &record, &fence).is_err());
        assert_eq!(yard.artifacts("producer").unwrap(), first);
        store.backend().finish(&fence, Some(&record), None).unwrap();
    }
}
