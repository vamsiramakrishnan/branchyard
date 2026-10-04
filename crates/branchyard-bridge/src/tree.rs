//! Directory trees as protocol frames: [`Frame::Entry`] for each directory,
//! regular file and symbolic link, a file's content as [`Frame::Data`]
//! ending with [`Frame::End`], and a final [`Frame::End`].
//!
//! Both ends treat the other's tree as untrusted. Entry paths must be
//! relative and made of plain components; a symbolic link is recreated as a
//! link and never followed, and nothing is written through one, so a tree
//! cannot place a file outside its root. Other file types (sockets, devices,
//! FIFOs) are skipped when sending. Owners are not kept; permission bits
//! are.

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use crate::protocol::{EntryKind, Frame, CHUNK};

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// `path` as a relative path of plain components, or an error.
pub fn relative(path: &[u8]) -> io::Result<PathBuf> {
    let path = Path::new(std::ffi::OsStr::from_bytes(path));
    let plain = !path.as_os_str().is_empty()
        && path.components().all(|c| matches!(c, Component::Normal(_)));
    if !plain {
        return Err(invalid(format!(
            "tree entry {} is not a relative path of plain components",
            path.display()
        )));
    }
    Ok(path.to_path_buf())
}

/// Send the tree under `root`, which must be a directory, sorted by path.
pub fn send(root: &Path, send: &mut dyn FnMut(Frame) -> io::Result<()>) -> io::Result<()> {
    let meta = fs::symlink_metadata(root)?;
    if !meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a directory", root.display()),
        ));
    }
    walk(root, Path::new(""), send)?;
    send(Frame::End)
}

fn walk(root: &Path, at: &Path, send: &mut dyn FnMut(Frame) -> io::Result<()>) -> io::Result<()> {
    let mut names: Vec<_> = fs::read_dir(root.join(at))?
        .map(|entry| entry.map(|e| e.file_name()))
        .collect::<io::Result<_>>()?;
    names.sort();
    for name in names {
        let relative = at.join(&name);
        let full = root.join(&relative);
        let meta = fs::symlink_metadata(&full)?;
        let path = relative.as_os_str().as_bytes().to_vec();
        let mode = meta.permissions().mode() & 0o7777;
        let kind = meta.file_type();
        if kind.is_symlink() {
            let target = fs::read_link(&full)?;
            send(Frame::Entry {
                kind: EntryKind::Symlink,
                path,
                mode,
                target: target.as_os_str().as_bytes().to_vec(),
            })?;
        } else if kind.is_dir() {
            send(Frame::Entry {
                kind: EntryKind::Dir,
                path,
                mode,
                target: Vec::new(),
            })?;
            walk(root, &relative, send)?;
        } else if kind.is_file() {
            send(Frame::Entry {
                kind: EntryKind::File,
                path,
                mode,
                target: Vec::new(),
            })?;
            let mut file = fs::File::open(&full)?;
            send_content(&mut file, send)?;
        }
    }
    Ok(())
}

/// Send a reader's content as [`Frame::Data`], then [`Frame::End`].
pub fn send_content(
    reader: &mut dyn Read,
    send: &mut dyn FnMut(Frame) -> io::Result<()>,
) -> io::Result<()> {
    let mut buf = vec![0u8; CHUNK];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => send(Frame::Data(buf[..n].to_vec()))?,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    send(Frame::End)
}

/// Write [`Frame::Data`] from `next` to `writer` until [`Frame::End`].
pub fn receive_content(
    writer: &mut dyn Write,
    next: &mut dyn FnMut() -> io::Result<Frame>,
) -> io::Result<()> {
    loop {
        match next()? {
            Frame::Data(data) => writer.write_all(&data)?,
            Frame::End => return writer.flush(),
            other => return Err(invalid(format!("expected file data, got {other:?}"))),
        }
    }
}

/// Make `root.join(relative)`'s parent a real directory chain, refusing to
/// pass through anything that is not a directory, such as a symbolic link
/// the tree created earlier.
fn parent_dirs(root: &Path, relative: &Path) -> io::Result<()> {
    let mut at = root.to_path_buf();
    let parents: Vec<_> = relative.components().collect();
    for component in &parents[..parents.len().saturating_sub(1)] {
        at.push(component);
        match fs::symlink_metadata(&at) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(invalid(format!(
                    "{} is not a directory; refusing to write through it",
                    at.display()
                )))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(&at)?,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Remove whatever is at `path` without following a symbolic link.
fn clear(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Write the tree from `next` under `root`, creating `root`. Existing
/// entries the tree names are replaced; others are left alone. Directory
/// permissions are applied last, so a read-only directory can still be
/// filled.
pub fn receive(root: &Path, next: &mut dyn FnMut() -> io::Result<Frame>) -> io::Result<()> {
    match fs::symlink_metadata(root) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            return Err(invalid(format!(
                "{} exists and is not a directory",
                root.display()
            )))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir_all(root)?,
        Err(error) => return Err(error),
    }
    let mut dirs = Vec::new();
    loop {
        let (kind, path, mode, target) = match next()? {
            Frame::End => break,
            Frame::Entry {
                kind,
                path,
                mode,
                target,
            } => (kind, path, mode, target),
            other => return Err(invalid(format!("expected a tree entry, got {other:?}"))),
        };
        let relative = relative(&path)?;
        parent_dirs(root, &relative)?;
        let full = root.join(&relative);
        match kind {
            EntryKind::Dir => {
                match fs::symlink_metadata(&full) {
                    Ok(meta) if meta.is_dir() => {}
                    _ => {
                        clear(&full)?;
                        fs::create_dir(&full)?;
                    }
                }
                dirs.push((full, mode));
            }
            EntryKind::Symlink => {
                clear(&full)?;
                symlink(std::ffi::OsStr::from_bytes(&target), &full)?;
            }
            EntryKind::File => {
                clear(&full)?;
                // Owner-only until the content is in, so a secret is never
                // readable by others while it arrives; its mode comes after.
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&full)?;
                receive_content(&mut file, next)?;
                file.set_permissions(fs::Permissions::from_mode(mode & 0o7777))?;
            }
        }
    }
    for (dir, mode) in dirs.into_iter().rev() {
        fs::set_permissions(&dir, fs::Permissions::from_mode(mode & 0o7777))?;
    }
    Ok(())
}

#[allow(clippy::let_underscore_must_use)] // tests: a panic is the failure report
#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "branchyard-bridge-tree-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn frames_of(root: &Path) -> VecDeque<Frame> {
        let mut frames = VecDeque::new();
        send(root, &mut |frame| {
            frames.push_back(frame);
            Ok(())
        })
        .unwrap();
        frames
    }

    fn replay(root: &Path, mut frames: VecDeque<Frame>) -> io::Result<()> {
        receive(root, &mut || {
            frames
                .pop_front()
                .ok_or_else(|| invalid("ran out of frames"))
        })
    }

    #[test]
    fn a_tree_round_trips_with_modes_and_links() {
        let dir = scratch("round-trip");
        let from = dir.join("from");
        fs::create_dir_all(from.join("sub/deeper")).unwrap();
        fs::write(from.join("plain.txt"), "plain").unwrap();
        fs::write(from.join("sub/tool.sh"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(from.join("sub/tool.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(from.join("sub/deeper/binary"), [0u8, 255, 1, 2]).unwrap();
        symlink("../plain.txt", from.join("sub/link")).unwrap();
        fs::write(from.join("big"), vec![7u8; 3 * CHUNK + 5]).unwrap();

        let to = dir.join("to");
        replay(&to, frames_of(&from)).unwrap();
        assert_eq!(fs::read(to.join("plain.txt")).unwrap(), b"plain");
        assert_eq!(fs::read(to.join("big")).unwrap(), vec![7u8; 3 * CHUNK + 5]);
        assert_eq!(
            fs::read(to.join("sub/deeper/binary")).unwrap(),
            [0u8, 255, 1, 2]
        );
        let mode = fs::metadata(to.join("sub/tool.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        assert_eq!(
            fs::read_link(to.join("sub/link")).unwrap(),
            Path::new("../plain.txt")
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_is_owner_only_until_its_content_is_in() {
        let dir = scratch("owner-only");
        let root = dir.join("root");
        fs::create_dir_all(&root).unwrap();
        let mut frames = VecDeque::from([
            Frame::Entry {
                kind: EntryKind::File,
                path: b"open.txt".to_vec(),
                mode: 0o644,
                target: Vec::new(),
            },
            Frame::Data(b"secret".to_vec()),
            Frame::End,
            Frame::End,
        ]);
        let mut seen = Vec::new();
        let path = root.join("open.txt");
        receive(&root, &mut || {
            // While its content arrives: before the file's own `End` and
            // the tree's.
            if frames.len() >= 2 {
                if let Ok(meta) = fs::metadata(&path) {
                    seen.push(meta.permissions().mode() & 0o777);
                }
            }
            frames
                .pop_front()
                .ok_or_else(|| invalid("ran out of frames"))
        })
        .unwrap();
        assert!(!seen.is_empty(), "the file existed while it was received");
        assert!(seen.iter().all(|mode| *mode == 0o600), "{seen:?}");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644,
            "then it gets its own mode"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_tree_cannot_escape_its_root() {
        let dir = scratch("escape");
        let root = dir.join("root");
        let outside = dir.join("outside");
        fs::create_dir_all(&outside).unwrap();
        for bad in [&b"../x"[..], b"/etc/x", b"a/../../x", b""] {
            let frames = VecDeque::from([
                Frame::Entry {
                    kind: EntryKind::File,
                    path: bad.to_vec(),
                    mode: 0o644,
                    target: Vec::new(),
                },
                Frame::Data(b"x".to_vec()),
                Frame::End,
                Frame::End,
            ]);
            assert!(replay(&root, frames).is_err(), "{:?}", bad);
        }
        // A link to a directory outside, then a file through the link.
        let frames = VecDeque::from([
            Frame::Entry {
                kind: EntryKind::Symlink,
                path: b"escape".to_vec(),
                mode: 0o777,
                target: outside.as_os_str().as_bytes().to_vec(),
            },
            Frame::Entry {
                kind: EntryKind::File,
                path: b"escape/owned".to_vec(),
                mode: 0o644,
                target: Vec::new(),
            },
            Frame::Data(b"x".to_vec()),
            Frame::End,
            Frame::End,
        ]);
        assert!(replay(&root, frames).is_err());
        assert!(!outside.join("owned").exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
