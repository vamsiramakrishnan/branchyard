//! Whether `by` may act on the `BRANCHYARD_ROOT` it inherited.
//!
//! The engine gives every local harness `BRANCHYARD_ROOT` (the yard's
//! repository) and `BRANCHYARD_WORKTREE` (the branch's worktree), and every
//! process the harness starts inherits them: a test suite run inside a
//! turn launches `by` in throwaway repositories with them still set. The
//! rule, for a `by` run in directory `cwd` with `BRANCHYARD_ROOT` set:
//!
//! 1. `cwd`'s repository is the nearest ancestor of `cwd` (or `cwd`
//!    itself) holding a `.git`. A `.branchyard` directory crossed before
//!    one is found ends the search with no repository: what is under it
//!    (temporary directories, state) is Branchyard's, not the
//!    repository's, though git counts it as part of the working tree. A
//!    worktree under it has its own `.git` and is found first.
//! 2. `BRANCHYARD_ROOT` applies when `cwd` is inside `BRANCHYARD_WORKTREE`,
//!    the turn's own worktree, and `cwd`'s repository is none or that
//!    worktree or an ancestor of it (not a repository nested inside it).
//! 3. Otherwise it applies when `cwd`'s repository keeps its git
//!    directory in `BRANCHYARD_ROOT`'s common git directory: the root
//!    itself, one of its linked worktrees, or a submodule of either (whose
//!    git directory is `.git/[worktrees/<b>/]modules/<x>`).
//! 4. Otherwise `by` refuses, rather than act on the inherited yard or, as
//!    a person, on the one it was run in.
//!
//! Paths are compared canonicalized. Delegation (`BRANCHYARD_DELEGATION`)
//! is reached through the same root, so the rule covers it too.

use std::fs;
use std::path::{Path, PathBuf};

use branchyard::{ENV_ROOT, ENV_WORKTREE};

/// The `BRANCHYARD_ROOT` this `by` may act on: `None` when it is unset,
/// an error when the current directory is outside both it and this turn's
/// worktree.
pub(crate) fn root() -> Result<Option<PathBuf>, branchyard::Error> {
    let var = |name: &str| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let Some(root) = var(ENV_ROOT) else {
        return Ok(None);
    };
    let cwd = std::env::current_dir().map_err(branchyard::Error::Io)?;
    let worktree = var(ENV_WORKTREE);
    if applies(&root, worktree.as_deref(), &cwd) {
        return Ok(Some(root));
    }
    Err(branchyard::Error::Denied(format!(
        "{ENV_ROOT} is {}, but by runs in {}, which is neither in that repository nor in \
         this turn's worktree ({ENV_WORKTREE}); unset the BRANCHYARD_ variables to act on \
         the repository here",
        root.display(),
        cwd.display()
    )))
}

/// Whether a `by` run in `cwd` acts on `root`; see the module comment.
pub(crate) fn applies(root: &Path, worktree: Option<&Path>, cwd: &Path) -> bool {
    let here = canonical(cwd);
    let repository = repository(&here);
    if let Some(worktree) = worktree.map(canonical) {
        let own = repository.as_ref().is_none_or(|r| worktree.starts_with(r));
        if here.starts_with(&worktree) && own {
            return true;
        }
    }
    match (repository, git_dir(&canonical(root))) {
        (Some(repository), Some(root)) => {
            own_git_dir(&repository).is_some_and(|dir| dir.starts_with(&root))
        }
        _ => false,
    }
}

/// The working tree `dir` is in, by rule 1 of the module comment.
fn repository(dir: &Path) -> Option<PathBuf> {
    for ancestor in dir.ancestors() {
        if ancestor.join(".git").exists() {
            return Some(ancestor.to_path_buf());
        }
        if ancestor.file_name().is_some_and(|n| n == ".branchyard") {
            return None;
        }
    }
    None
}

/// The git directory of the working tree at `top`: its `.git` directory,
/// or the one its `.git` file names (for a linked worktree or a submodule).
fn own_git_dir(top: &Path) -> Option<PathBuf> {
    let dot_git = top.join(".git");
    if dot_git.is_dir() {
        return Some(canonical(&dot_git));
    }
    let text = fs::read_to_string(&dot_git).ok()?;
    Some(canonical(
        &top.join(text.trim().strip_prefix("gitdir:")?.trim()),
    ))
}

/// The common git directory of the working tree at `top`: its own git
/// directory, or the one that directory's `commondir` names (for a linked
/// worktree).
fn git_dir(top: &Path) -> Option<PathBuf> {
    let own = own_git_dir(top)?;
    let common = match fs::read_to_string(own.join("commondir")) {
        Ok(common) => own.join(common.trim()),
        Err(_) => own,
    };
    Some(canonical(&common))
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A yard at `<dir>/yard` with a linked worktree under
    /// `.branchyard/worktrees/b`, its temporary directory, and a second
    /// repository nested in that.
    struct Layout {
        _dir: tempfile::TempDir,
        root: PathBuf,
        worktree: PathBuf,
        tmp: PathBuf,
        other: PathBuf,
    }

    fn layout() -> Layout {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("yard");
        let worktree = root.join(".branchyard/worktrees/b");
        let tmp = root.join(".branchyard/tmp/b");
        let other = tmp.join("test/repo");
        let own = root.join(".git/worktrees/b");
        for d in [
            &worktree,
            &tmp,
            &other.join(".git"),
            &own,
            &root.join("src"),
        ] {
            fs::create_dir_all(d).unwrap();
        }
        fs::write(own.join("commondir"), "../..\n").unwrap();
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", own.display()),
        )
        .unwrap();
        Layout {
            _dir: dir,
            root,
            worktree,
            tmp,
            other,
        }
    }

    #[test]
    fn the_root_applies_in_its_repository_and_its_worktrees() {
        let l = layout();
        for cwd in [&l.root, &l.root.join("src"), &l.worktree] {
            assert!(applies(&l.root, None, cwd), "{}", cwd.display());
            assert!(
                applies(&l.root, Some(&l.worktree), cwd),
                "{}",
                cwd.display()
            );
        }
    }

    #[test]
    fn the_root_does_not_apply_in_another_repository_even_one_inside_it() {
        let l = layout();
        assert!(!applies(&l.root, Some(&l.worktree), &l.other));
        assert!(!applies(&l.root, None, &l.other));
        // A repository nested in the turn's own worktree is not the turn's.
        let nested = l.worktree.join("target/tmp/repo");
        fs::create_dir_all(nested.join(".git")).unwrap();
        assert!(!applies(&l.root, Some(&l.worktree), &nested));
    }

    #[test]
    fn under_branchyard_is_no_repository_and_outside_any_is_refused() {
        let l = layout();
        // Git would call this part of the root's working tree.
        assert!(!applies(&l.root, Some(&l.worktree), &l.tmp));
        assert!(!applies(&l.root, None, &l.root.join(".branchyard")));
        let elsewhere = tempfile::tempdir().unwrap();
        assert!(!applies(&l.root, Some(&l.worktree), elsewhere.path()));
    }

    #[test]
    fn the_turns_own_worktree_applies_without_a_git_directory() {
        // A worktree copied somewhere with no `.git` of its own.
        let dir = tempfile::tempdir().unwrap();
        let worktree = dir.path().join("work");
        fs::create_dir_all(worktree.join("src")).unwrap();
        let root = dir.path().join("yard");
        assert!(applies(&root, Some(&worktree), &worktree.join("src")));
        assert!(!applies(&root, None, &worktree.join("src")));
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(["-c", "protocol.file.allow=always"])
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    #[test]
    fn the_root_applies_in_a_submodule_of_it_or_of_its_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        let root = dir.path().join("yard");
        for repo in [&sub, &root] {
            fs::create_dir_all(repo).unwrap();
            git(repo, &["init", "-q", "-b", "main"]);
            git(repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        }
        git(
            &root,
            &["submodule", "add", "-q", sub.to_str().unwrap(), "x"],
        );
        git(&root, &["commit", "-q", "-m", "submodule"]);
        let worktree = root.join(".branchyard/worktrees/b");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "b",
                worktree.to_str().unwrap(),
            ],
        );
        git(&worktree, &["submodule", "update", "-q", "--init"]);
        // The worktree's submodule keeps its git directory under
        // `.git/worktrees/b/modules/x`, the root's under `.git/modules/x`.
        assert!(applies(&root, Some(&worktree), &worktree.join("x")));
        assert!(applies(&root, None, &worktree.join("x")));
        assert!(applies(&root, None, &root.join("x")));
        // The submodule's own repository is not the root's.
        assert!(!applies(&root, Some(&worktree), &sub));
    }
}
