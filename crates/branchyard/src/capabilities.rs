//! Source-free skills with progressive discovery for every task, independent of delegation.

use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use branchyard_provision::{apply, skills, Edit, Plan};
use serde::Deserialize;

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
    pub written: Vec<String>,
}

/// A separate cache per branch avoids concurrent writers in a shared HOME.
pub(crate) fn local_home(state: &Path, branch: &str) -> Result<PathBuf, String> {
    let mut path = state
        .canonicalize()
        .map_err(|e| format!("could not resolve document skill cache root: {e}"))?;
    let branch_id = blake3::hash(branch.as_bytes()).to_hex();
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

/// Install the embedded payload on the host, naming paths as the guest sees them.
pub(crate) fn stage(host: &Path, guest: &Path) -> Result<Staged, String> {
    let catalog: Catalog = serde_json::from_str(skills::CAPABILITIES)
        .map_err(|e| format!("invalid embedded capabilities: {e}"))?;
    let mut plan = Plan::default();
    for capability in catalog.skills {
        let skill = skills::skill(&capability.id)
            .ok_or_else(|| format!("embedded skill {} is missing", capability.id))?;
        let directory = format!("{DIRECTORY}/{}", skill.name);
        for (path, content) in skill.files {
            plan.edit(
                &format!("{directory}/{path}"),
                false,
                Edit::Put((*content).into()),
            );
        }
    }
    let index_path = format!("{DIRECTORY}/INDEX.md");
    plan.edit(&index_path, false, Edit::Put(skills::INDEX.into()));
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
        written: applied.written,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_free_payload_uses_guest_paths_and_repeated_staging_is_unchanged() {
        let host = tempfile::tempdir().unwrap();
        let guest = Path::new("/guest/home");
        let first = stage(host.path(), guest).unwrap();
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
        assert!(stage(host.path(), guest).unwrap().written.is_empty());
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
        assert!(stage(state.path(), Path::new("/home/guest"))
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
        assert!(stage(&home, &home).is_ok());
        assert_eq!(
            local_home(&linked.path().join("state"), "branch").unwrap(),
            home
        );
    }
}
