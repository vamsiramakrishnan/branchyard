//! Source-free skills with progressive discovery for every task, independent of delegation.
//!
//! Before each turn the catalogued built-in skills, and the repository's
//! skills when the branch stages them (`crate::skills`), are written under
//! `.branchyard/capabilities/` of the branch's private home, or of its
//! cache under the state directory when it has none ([`cache`]). Repository
//! skills go in `repository/<name>/` beside the built-ins and are listed
//! under a generated Custom topic; files a set no longer has are removed,
//! so the tree is the turn's set whatever an earlier turn left.

use std::collections::BTreeSet;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use branchyard_provision::skillset::{self, SkillEntry};
use branchyard_provision::{apply, skills, Edit, Plan};
use serde::Deserialize;

use crate::skills::RepositorySet;

const DIRECTORY: &str = ".branchyard/capabilities";

#[derive(Deserialize)]
struct Catalog {
    skills: Vec<Capability>,
}

#[derive(Deserialize)]
struct Capability {
    id: String,
}

#[derive(Debug)]
pub(crate) struct Staged {
    pub instruction: String,
    /// Files written or removed, relative to the home.
    pub written: Vec<String>,
    /// The built-in skills staged, in catalog order.
    pub built_in: Vec<SkillEntry>,
}

/// The directory of the state directory `state` holding the skills of
/// `branch`, a branch without a private home; removed with the branch.
pub(crate) fn cache(state: &Path, branch: &str) -> PathBuf {
    state.join("capabilities").join(branch_id(branch))
}

/// The name of `branch`'s cache directory: BLAKE3 of its name, in hex.
fn branch_id(branch: &str) -> String {
    blake3::hash(branch.as_bytes()).to_hex().to_string()
}

/// A separate cache per branch avoids concurrent writers in a shared HOME.
pub(crate) fn local_home(state: &Path, branch: &str) -> Result<PathBuf, String> {
    let mut path = state
        .canonicalize()
        .map_err(|e| format!("could not resolve document skill cache root: {e}"))?;
    let branch_id = branch_id(branch);
    for name in ["capabilities", branch_id.as_str()] {
        path.push(name);
        match std::fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("could not create document skill cache: {error}")),
        }
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|e| format!("could not inspect document skill cache: {e}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(format!(
                "document skill cache {} is not a plain directory; refusing to follow symbolic links",
                path.display()
            ));
        }
    }
    Ok(path)
}

/// The built-in skills every turn stages, in catalog order.
pub(crate) fn catalogued() -> Result<Vec<&'static skills::Skill>, String> {
    let catalog: Catalog = serde_json::from_str(skills::CAPABILITIES)
        .map_err(|e| format!("invalid embedded capabilities: {e}"))?;
    catalog
        .skills
        .iter()
        .map(|capability| {
            skills::skill(&capability.id)
                .ok_or_else(|| format!("embedded skill {} is missing", capability.id))
        })
        .collect()
}

/// Install the embedded payload, and `repository`'s skills when given, on
/// the host, naming paths as the guest sees them.
pub(crate) fn stage(
    host: &Path,
    guest: &Path,
    repository: Option<&RepositorySet>,
) -> Result<Staged, String> {
    let mut plan = Plan::default();
    let mut built_in = Vec::new();
    for skill in catalogued()? {
        let directory = format!("{DIRECTORY}/{}", skill.name);
        for (path, content) in skill.files {
            plan.edit(
                &format!("{directory}/{path}"),
                false,
                Edit::Put((*content).into()),
            );
        }
        built_in.push(skillset::built_in_entry(skill));
    }
    let index_path = format!("{DIRECTORY}/INDEX.md");
    let custom_path = format!("{DIRECTORY}/{}", skillset::CUSTOM_TOPIC);
    let repository_dir = format!("{DIRECTORY}/{}", skillset::REPOSITORY_ROOT);
    let mut custom = BTreeSet::new();
    match repository {
        None => {
            plan.edit(&index_path, false, Edit::Put(skills::INDEX.into()));
            plan.edit(&custom_path, false, Edit::Remove);
        }
        Some(set) => {
            let index = skillset::custom_index(skills::INDEX, &set.skills, &set.commit)?;
            plan.edit(&index_path, false, Edit::Put(index.root));
            plan.edit(&custom_path, false, Edit::Put(index.topic));
            for skill in &set.skills {
                for (path, content) in &skill.files {
                    let path = format!("{repository_dir}/{}/{path}", skill.name);
                    plan.edit(&path, false, Edit::Put(content.clone()));
                    custom.insert(path);
                }
            }
        }
    }
    for stale in files_under(host, &repository_dir)? {
        if !custom.contains(&stale) {
            plan.edit(&stale, false, Edit::Remove);
        }
    }
    if !skills::UPSTREAM_LOCK.is_empty() {
        plan.edit(
            &format!("{DIRECTORY}/upstream.lock.json"),
            false,
            Edit::Put(skills::UPSTREAM_LOCK.into()),
        );
    }
    for (path, content) in skills::TOPIC_INDEXES {
        plan.edit(
            &format!("{DIRECTORY}/{path}"),
            false,
            Edit::Put((*content).into()),
        );
    }
    let applied = apply::apply(&plan, host)
        .map_err(|e| format!("could not install document capabilities: {e}"))?;
    Ok(Staged {
        instruction: format!(
            "Skills and tooling are available. Read {} and load only the relevant topic and skill. Keep your native planning and subagents.",
            guest.join(index_path).display()
        ),
        written: applied.written.into_iter().chain(applied.removed).collect(),
        built_in,
    })
}

/// The regular files under `home`'s `dir`, relative to `home`, without
/// following symbolic links: none when `dir`, or a directory on the way to
/// it, is missing, a link or not a directory (the harness can write its
/// home; [`apply::apply`] then refuses to write through it). Removing them
/// goes through [`apply::apply`] too.
fn files_under(home: &Path, dir: &str) -> Result<Vec<String>, String> {
    let fail = |e: io::Error| format!("could not list the staged repository skills: {e}");
    let mut on_the_way = home.to_path_buf();
    for part in dir.split('/') {
        on_the_way.push(part);
        match std::fs::symlink_metadata(&on_the_way) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Ok(Vec::new()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(fail(e)),
        }
    }
    let mut found = Vec::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(relative) = pending.pop() {
        let entries = match std::fs::read_dir(home.join(&relative)) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(fail(e)),
        };
        for entry in entries {
            let entry = entry.map_err(fail)?;
            let kind = entry.file_type().map_err(fail)?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let path = format!("{relative}/{name}");
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file() {
                found.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_free_payload_uses_guest_paths_and_repeated_staging_is_unchanged() {
        let host = tempfile::tempdir().unwrap();
        let guest = Path::new("/guest/home");
        let first = stage(host.path(), guest, None).unwrap();
        assert!(!first.written.is_empty());
        assert!(first
            .instruction
            .contains("/guest/home/.branchyard/capabilities/INDEX.md"));
        let root = host.path().join(DIRECTORY);
        let index = std::fs::read_to_string(root.join("INDEX.md")).unwrap();
        assert_eq!(index, skills::INDEX);
        assert!(index.len() <= 4096);
        assert!(first.instruction.len() < 512);
        assert!(!first.instruction.contains("SKILL.md"));
        for (path, content) in skills::TOPIC_INDEXES {
            assert_eq!(std::fs::read_to_string(root.join(path)).unwrap(), *content);
        }
        let catalog: Catalog = serde_json::from_str(skills::CAPABILITIES).unwrap();
        for capability in catalog.skills {
            let directory = root.join(capability.id);
            assert!(directory.join("INDEX.md").is_file());
            assert!(directory.join("SKILL.md").is_file());
        }
        let mut directories = vec![root.clone()];
        while let Some(directory) = directories.pop() {
            for entry in std::fs::read_dir(&directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    directories.push(path);
                } else if path.file_name().is_some_and(|name| name == "INDEX.md") {
                    let text = std::fs::read_to_string(&path).unwrap();
                    assert!(text.len() <= 8192);
                    for line in text.lines() {
                        let Some((_, target)) = line.split_once("](") else {
                            continue;
                        };
                        let relative = target.split_once(')').unwrap().0;
                        assert!(!Path::new(relative).is_absolute());
                        let resolved = path
                            .parent()
                            .unwrap()
                            .join(relative)
                            .canonicalize()
                            .unwrap();
                        assert!(resolved.starts_with(root.canonicalize().unwrap()));
                        assert!(resolved.is_file(), "{} links to {relative}", path.display());
                    }
                }
            }
        }
        assert!(!index.contains(&host.path().display().to_string()));
        let helper = host
            .path()
            .join(DIRECTORY)
            .join("artifact-tools/scripts/run.py");
        let output = std::process::Command::new("python3")
            .arg(helper)
            .arg("doctor")
            .env("HOME", host.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(report.is_object());
        assert!(stage(host.path(), guest, None).unwrap().written.is_empty());
    }

    fn repository_set(texts: &[(&str, &str)]) -> RepositorySet {
        let skills = texts
            .iter()
            .map(|(name, extra)| {
                let mut files = vec![(
                    "SKILL.md".to_owned(),
                    format!("---\nname: {name}\ndescription: The {name} skill.\n---\n"),
                )];
                if !extra.is_empty() {
                    files.push(((*extra).to_owned(), "x".to_owned()));
                }
                skillset::Skill::new(name, files).unwrap()
            })
            .collect::<Vec<_>>();
        let entries: Vec<SkillEntry> = skills
            .iter()
            .map(|s| s.entry(skillset::Layer::Repository))
            .collect();
        RepositorySet {
            commit: "0123456789abcdef".into(),
            digest: skillset::set_digest(&entries),
            skills,
        }
    }

    #[test]
    fn repository_skills_are_staged_beside_the_built_ins_and_what_a_set_lost_goes() {
        let host = tempfile::tempdir().unwrap();
        let root = host.path().join(DIRECTORY);
        let set = repository_set(&[("style", "references/a.md"), ("tone", "")]);
        let first = stage(host.path(), host.path(), Some(&set)).unwrap();
        assert_eq!(first.built_in.len(), catalogued().unwrap().len());
        assert!(root.join("repository/style/references/a.md").is_file());
        assert!(root.join("repository/tone/SKILL.md").is_file());
        let index = std::fs::read_to_string(root.join("INDEX.md")).unwrap();
        assert!(index.contains("[Custom](topics/custom/INDEX.md)"));
        assert!(root.join(skillset::CUSTOM_TOPIC).is_file());
        assert!(stage(host.path(), host.path(), Some(&set))
            .unwrap()
            .written
            .is_empty());

        let smaller = repository_set(&[("style", "")]);
        let second = stage(host.path(), host.path(), Some(&smaller)).unwrap();
        assert!(!root.join("repository/style/references/a.md").exists());
        assert!(!root.join("repository/tone/SKILL.md").exists());
        assert!(second
            .written
            .contains(&format!("{DIRECTORY}/repository/tone/SKILL.md")));

        let none = stage(host.path(), host.path(), None).unwrap();
        assert!(!root.join("repository/style/SKILL.md").exists());
        assert!(!root.join(skillset::CUSTOM_TOPIC).exists());
        assert_eq!(
            std::fs::read_to_string(root.join("INDEX.md")).unwrap(),
            skills::INDEX
        );
        assert!(none
            .written
            .contains(&format!("{DIRECTORY}/{}", skillset::CUSTOM_TOPIC)));
    }

    #[test]
    fn a_linked_repository_directory_in_the_home_is_not_walked() {
        let host = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("keep.txt"), "mine").unwrap();
        stage(host.path(), host.path(), None).unwrap();
        std::os::unix::fs::symlink(
            outside.path(),
            host.path().join(DIRECTORY).join(skillset::REPOSITORY_ROOT),
        )
        .unwrap();
        assert!(files_under(host.path(), &format!("{DIRECTORY}/repository"))
            .unwrap()
            .is_empty());
        stage(host.path(), host.path(), None).unwrap();
        assert!(outside.path().join("keep.txt").is_file());
    }

    #[test]
    fn branch_caches_are_distinct_and_symlink_directories_are_refused() {
        let state = tempfile::tempdir().unwrap();
        assert_ne!(
            local_home(state.path(), "a").unwrap(),
            local_home(state.path(), "b").unwrap()
        );
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), state.path().join(".branchyard")).unwrap();
        assert!(stage(state.path(), Path::new("/home/guest"), None)
            .unwrap_err()
            .contains("symbolic link"));
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn local_cache_rejects_linked_ancestors_and_accepts_a_trusted_outer_root() {
        let state = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), state.path().join("capabilities")).unwrap();
        assert!(local_home(state.path(), "branch")
            .unwrap_err()
            .contains("symbolic links"));
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);

        let linked = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), linked.path().join("state")).unwrap();
        let home = local_home(&linked.path().join("state"), "branch").unwrap();
        assert!(home.starts_with(outside.path().canonicalize().unwrap()));
        assert!(stage(&home, &home, None).is_ok());
        assert_eq!(
            local_home(&linked.path().join("state"), "branch").unwrap(),
            home
        );
    }
}
