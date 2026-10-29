//! Source-free document skills for every task, independent of delegation.

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
    description: String,
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
    let mut index = String::from("# Document capabilities\n\nRead the skill for the requested format. These skills support your own planning and delegation.\n\n");
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
        index.push_str(&format!(
            "- {}: {}\n  Skill: {}\n",
            skill.name,
            capability.description,
            guest.join(&directory).join("SKILL.md").display()
        ));
    }
    index.push_str(&format!(
        "\nToolkit: python3 {}\nRun doctor before choosing a format operation. Read artifact-tools for runtime setup and manifests. Google operations require the granted connector and its upstream account permissions.\n",
        branchyard_recipe::quote(&guest.join(DIRECTORY).join("artifact-tools/scripts/run.py").display().to_string())
    ));
    let index_path = format!("{DIRECTORY}/INDEX.md");
    plan.edit(&index_path, false, Edit::Put(index));
    let applied = apply::apply(&plan, host)
        .map_err(|e| format!("could not install document capabilities: {e}"))?;
    Ok(Staged {
        instruction: format!(
            "Document skills and tooling are available. When working with documents, spreadsheets, PDFs or presentations, read {} and load the relevant skill. Keep your native planning and subagents.",
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
        let index = std::fs::read_to_string(host.path().join(DIRECTORY).join("INDEX.md")).unwrap();
        for id in [
            "docx",
            "xlsx",
            "pdf",
            "pptx",
            "google-docs",
            "google-sheets",
            "google-slides",
            "parallel-deck",
        ] {
            assert!(index.contains(&format!("/guest/home/{DIRECTORY}/{id}/SKILL.md")));
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
