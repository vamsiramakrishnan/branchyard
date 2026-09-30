// Derived from Orca (https://github.com/stablyai/orca) at
// 280733273545f0b3eeedc1be54b14d406239030e: src/main/ipc/worktree-symlinks.ts
// and src/main/ipc/worktree-apfs-clone.ts. Copyright (c) 2026 Lovecast Inc.
// Licensed under the MIT License; see vendor/orca/LICENSE.
//
// Modified for Branchyard: translated from TypeScript to Rust. The modes
// follow Orca (a copy clones where the filesystem can and copies
// otherwise; a shared path is always a symbolic link, never a clone), as
// do the path checks, the skip of a missing source or an existing target,
// and the removal of only what is a link. Cloning is Linux's FICLONE per
// file as well as macOS's clonefile (through `cp -c`, as Orca does, when
// source and target are on one device, without Orca's df/diskutil probe);
// a directory is walked here, keeping symbolic links inside it verbatim,
// rather than handed to `cp -R`; Windows junctions and Orca's copy budget
// are not ported; each path's method, or why it was skipped, is returned
// instead of logged.

//! Materializing paths from one directory tree into a worktree: cloned
//! (copy-on-write) where the filesystem supports it, copied otherwise, or
//! linked. Prepared environments (`branchyard::environments`) restore what
//! setup produced this way, and `.worktreeinclude` copies use it too.
//!
//! - [`Mode::Copy`]: each path gets its own copy. A regular file is cloned
//!   with `FICLONE` (Linux: Btrfs, XFS, bcachefs, OverlayFS over one of
//!   them) or `clonefile` (macOS APFS), and copied byte for byte when the
//!   filesystem cannot clone. A directory is walked, each file the same
//!   way, each symbolic link inside it recreated verbatim.
//! - [`Mode::Share`]: each path becomes a symbolic link to the source, so
//!   one install serves every worktree (`node_modules`). Never cloned: an
//!   independent copy would defeat the point.
//!
//! Nothing is overwritten: a target that exists (even a dangling link) is
//! left alone and reported, and a missing source is skipped silently, as
//! Orca does (`node_modules` before an install).

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

/// How one path reached the target tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// Every file was cloned (copy-on-write).
    Clone,
    /// At least one file was copied byte for byte.
    Copy,
    /// A symbolic link to the source.
    Link,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Clone => "clone",
            Method::Copy => "copy",
            Method::Link => "link",
        }
    }
}

/// What [`materialize`] does with each path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Its own copy: cloned where possible, else copied.
    Copy,
    /// A symbolic link to the source, always.
    Share,
}

/// One path placed in the target tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placed {
    pub path: String,
    pub method: Method,
}

/// What [`materialize`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Materialized {
    pub placed: Vec<Placed>,
    /// Paths not placed, each with why.
    pub skipped: Vec<(String, String)>,
}

impl Materialized {
    /// The method for the whole set: a link if any path was linked and none
    /// copied, a copy if any file was copied, else a clone.
    pub fn method(&self) -> Option<Method> {
        let methods: Vec<Method> = self.placed.iter().map(|p| p.method).collect();
        if methods.is_empty() {
            None
        } else if methods.contains(&Method::Copy) {
            Some(Method::Copy)
        } else if methods.iter().all(|m| *m == Method::Link) {
            Some(Method::Link)
        } else {
            Some(Method::Clone)
        }
    }
}

/// `path` as a safe relative path (no root, no `..`, no `.` or empty
/// segment), with `/` separators, or `None`.
pub fn safe_relative(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() || trimmed.starts_with('/') {
        return None;
    }
    let ok = trimmed
        .split('/')
        .all(|s| !s.is_empty() && s != "." && s != "..")
        && Path::new(trimmed)
            .components()
            .all(|c| matches!(c, Component::Normal(_)));
    ok.then(|| trimmed.to_owned())
}

/// Place each of `paths` (relative) from `source_root` at the same path
/// under `target_root`, per `mode`. Per-path failures are isolated: each is
/// skipped with its reason and the rest go on.
pub fn materialize(
    source_root: &Path,
    target_root: &Path,
    paths: &[String],
    mode: Mode,
) -> Materialized {
    let mut done = Materialized::default();
    for raw in paths {
        let Some(rel) = safe_relative(raw) else {
            done.skipped
                .push((raw.clone(), "is not a safe relative path".into()));
            continue;
        };
        let source = source_root.join(&rel);
        let target = target_root.join(&rel);
        // A missing source has nothing to place.
        let Ok(source_meta) = fs::symlink_metadata(&source) else {
            continue;
        };
        if fs::symlink_metadata(&target).is_ok() {
            done.skipped.push((rel, "the target already exists".into()));
            continue;
        }
        if let Some(parent) = target.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                done.skipped
                    .push((rel, format!("could not create its directory: {e}")));
                continue;
            }
        }
        let placed = match mode {
            Mode::Share => link(&source, &target).map(|()| Method::Link),
            Mode::Copy if source_meta.file_type().is_symlink() => {
                copy_link(&source, &target).map(|()| Method::Copy)
            }
            Mode::Copy => clone_or_copy(&source, &target),
        };
        match placed {
            Ok(method) => done.placed.push(Placed { path: rel, method }),
            Err(e) => done.skipped.push((rel, e.to_string())),
        }
    }
    done
}

/// Remove each of `paths` under `root` that is a symbolic link, and only
/// those: a file or directory someone put there is left alone. Run before
/// a worktree whose shared paths are links is deleted.
pub fn remove_links(root: &Path, paths: &[String]) -> Vec<String> {
    let mut removed = Vec::new();
    for raw in paths {
        let Some(rel) = safe_relative(raw) else {
            continue;
        };
        let target = root.join(&rel);
        if fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink())
            && fs::remove_file(&target).is_ok()
        {
            removed.push(rel);
        }
    }
    removed
}

fn link(source: &Path, target: &Path) -> io::Result<()> {
    let absolute = match source.is_absolute() {
        true => source.to_path_buf(),
        false => std::env::current_dir()?.join(source),
    };
    std::os::unix::fs::symlink(absolute, target)
}

fn copy_link(source: &Path, target: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(fs::read_link(source)?, target)
}

/// Copy `source` (a file or a directory tree) to `target`, which must not
/// exist, cloning each file where the filesystem can. [`Method::Clone`]
/// when every file was cloned.
pub fn clone_or_copy(source: &Path, target: &Path) -> io::Result<Method> {
    let meta = fs::symlink_metadata(source)?;
    if meta.is_dir() {
        let mut method = Method::Clone;
        copy_dir(source, target, &mut method)?;
        Ok(method)
    } else if meta.file_type().is_symlink() {
        copy_link(source, target).map(|()| Method::Copy)
    } else {
        clone_file(source, target)
    }
}

fn copy_dir(source: &Path, target: &Path, method: &mut Method) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(source)?.permissions().mode();
    // Reserve the directory first, so a raced one is never merged into.
    fs::create_dir(target)?;
    let mut entries: Vec<PathBuf> = fs::read_dir(source)?
        .map(|e| e.map(|e| e.path()))
        .collect::<io::Result<_>>()?;
    entries.sort();
    for entry in entries {
        let name = entry.file_name().expect("a directory entry has a name");
        let to = target.join(name);
        let meta = fs::symlink_metadata(&entry)?;
        if meta.is_dir() {
            copy_dir(&entry, &to, method)?;
        } else if meta.file_type().is_symlink() {
            copy_link(&entry, &to)?;
        } else if meta.is_file() && clone_file(&entry, &to)? == Method::Copy {
            *method = Method::Copy;
        }
        // Sockets, fifos and devices are not carried.
    }
    fs::set_permissions(target, fs::Permissions::from_mode(mode))
}

/// Clone one regular file, or copy it when the filesystem cannot clone.
pub fn clone_file(source: &Path, target: &Path) -> io::Result<Method> {
    let method = match platform::clone(source, target) {
        Ok(()) => Method::Clone,
        // Never replace what someone else put there.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Err(e),
        Err(_) => {
            // Nothing half-made is left by a refused clone; a byte copy
            // follows. fs::copy keeps the permission bits.
            let _ = fs::remove_file(target);
            fs::copy(source, target)?;
            Method::Copy
        }
    };
    if let Ok(modified) = fs::metadata(source).and_then(|m| m.modified()) {
        if let Ok(file) = fs::OpenOptions::new().write(true).open(target) {
            let _ = file.set_modified(modified);
        }
    }
    Ok(method)
}

#[cfg(target_os = "linux")]
mod platform {
    use std::fs;
    use std::io;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::path::Path;

    /// `FICLONE`: the target shares the source's extents until either is
    /// written. Fails with `EOPNOTSUPP` or `EXDEV` where it cannot (ext4,
    /// tmpfs, two filesystems).
    pub fn clone(source: &Path, target: &Path) -> io::Result<()> {
        let from = fs::File::open(source)?;
        let mode = from.metadata()?.permissions().mode();
        let to = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode & 0o7777)
            .open(target)?;
        rustix::fs::ioctl_ficlone(&to, &from).map_err(io::Error::from)?;
        // The creation mode was masked by the umask; a copy keeps it.
        fs::set_permissions(target, fs::Permissions::from_mode(mode))
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::io;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;
    use std::process::{Command, Stdio};

    /// APFS `clonefile` through `cp -c`, as Orca does, only when the source
    /// and the target's directory are on one device: across volumes `cp
    /// -c` silently falls back to a full copy, which is not a clone.
    pub fn clone(source: &Path, target: &Path) -> io::Result<()> {
        let parent = target.parent().unwrap_or(Path::new("."));
        if std::fs::metadata(source)?.dev() != std::fs::metadata(parent)?.dev() {
            return Err(io::Error::other(
                "source and target are on different volumes",
            ));
        }
        let status = Command::new("/bin/cp")
            .arg("-c")
            .arg(source)
            .arg(target)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        match status.success() {
            true => Ok(()),
            false => Err(io::Error::other("cp -c failed")),
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    use std::io;
    use std::path::Path;

    pub fn clone(_: &Path, _: &Path) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("by-mat-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn copies_keep_modes_and_inner_links_and_never_overwrite() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tree("copy");
        let (from, to) = (dir.join("from"), dir.join("to"));
        fs::create_dir_all(from.join("node_modules/.bin")).unwrap();
        fs::write(from.join("node_modules/pkg.js"), "x").unwrap();
        fs::write(from.join("tool"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(from.join("tool"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("../pkg.js", from.join("node_modules/.bin/pkg")).unwrap();
        fs::create_dir_all(&to).unwrap();
        fs::write(to.join("tool"), "mine").unwrap();
        let done = materialize(
            &from,
            &to,
            &[
                "node_modules".into(),
                "tool".into(),
                "absent".into(),
                "../x".into(),
            ],
            Mode::Copy,
        );
        assert_eq!(done.placed.len(), 1, "{done:?}");
        assert_ne!(done.placed[0].method, Method::Link);
        assert_eq!(done.skipped.len(), 2, "{done:?}");
        assert_eq!(fs::read_to_string(to.join("tool")).unwrap(), "mine");
        assert_eq!(
            fs::read_link(to.join("node_modules/.bin/pkg")).unwrap(),
            Path::new("../pkg.js")
        );
        assert_eq!(
            fs::read_to_string(to.join("node_modules/.bin/pkg")).unwrap(),
            "x"
        );
        // A copy is independent of its source.
        fs::write(to.join("node_modules/pkg.js"), "changed").unwrap();
        assert_eq!(
            fs::read_to_string(from.join("node_modules/pkg.js")).unwrap(),
            "x"
        );
        let file = clone_or_copy(&from.join("tool"), &dir.join("tool2")).unwrap();
        assert_ne!(file, Method::Link);
        let mode = fs::metadata(dir.join("tool2"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn shared_paths_are_links_and_only_links_are_removed() {
        let dir = tree("share");
        let (from, to) = (dir.join("from"), dir.join("to"));
        fs::create_dir_all(from.join("node_modules")).unwrap();
        fs::create_dir_all(from.join("keep")).unwrap();
        fs::create_dir_all(to.join("keep")).unwrap();
        let done = materialize(
            &from,
            &to,
            &["node_modules".into(), "keep".into()],
            Mode::Share,
        );
        assert_eq!(done.method(), Some(Method::Link));
        assert_eq!(
            fs::read_link(to.join("node_modules")).unwrap(),
            from.join("node_modules")
        );
        let removed = remove_links(&to, &["node_modules".into(), "keep".into()]);
        assert_eq!(removed, ["node_modules"]);
        assert!(to.join("keep").is_dir());
        assert!(from.join("node_modules").is_dir());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsafe_paths_are_refused() {
        for bad in ["", "/etc", "../x", "a/../b", "./a", "a//b"] {
            assert_eq!(safe_relative(bad), None, "{bad}");
        }
        assert_eq!(safe_relative("a/b/"), Some("a/b".into()));
    }
}
