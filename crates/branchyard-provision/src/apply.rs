//! Carry out a [`Plan`]'s file edits in a home directory.
//!
//! Each file is read, edited in memory, and written through a temporary
//! file in the same directory and a rename, so a harness never reads half a
//! file and a symbolic link at the target is replaced rather than followed.
//! Paths are relative to the home and never leave it: a `..`, an absolute
//! path, or a symbolic link on the way is refused, so a link a harness left
//! in its home cannot redirect a secret into the worktree. A file holding a
//! secret is written with mode 0600; other files keep their mode, or get
//! 0644 when new. Directories are created 0700. A file whose content and
//! mode are already right is not touched, so applying a plan twice changes
//! nothing the second time.
//!
//! Errors name the file and the problem, never its content.

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use crate::edit::apply_all;
use crate::{json_text, Credential, Edit, FileEdit, Installed, JsonEdit, Plan, INSTALLED_PATH};

/// Largest file the executor reads before editing it.
const MAX_FILE: u64 = 16 << 20;

/// What applying a plan did, by relative path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    pub written: Vec<String>,
    pub removed: Vec<String>,
    pub unchanged: Vec<String>,
}

/// A file the executor could not edit.
#[derive(Debug)]
pub struct ApplyError {
    pub path: String,
    pub reason: String,
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.reason)
    }
}

impl std::error::Error for ApplyError {}

/// Apply `plan`'s files in `home`, which must be an existing directory.
pub fn apply(plan: &Plan, home: &Path) -> Result<Applied, ApplyError> {
    let meta = fs::symlink_metadata(home).map_err(|e| ApplyError {
        path: home.display().to_string(),
        reason: format!("the home is not usable: {e}"),
    })?;
    if !meta.is_dir() {
        return Err(ApplyError {
            path: home.display().to_string(),
            reason: "the home is not a directory".into(),
        });
    }
    let mut applied = Applied::default();
    for file in &plan.files {
        let fail = |reason: String| ApplyError {
            path: file.path.clone(),
            reason,
        };
        match apply_file(home, file).map_err(fail)? {
            Outcome::Written => applied.written.push(file.path.clone()),
            Outcome::Removed => applied.removed.push(file.path.clone()),
            Outcome::Unchanged => applied.unchanged.push(file.path.clone()),
        }
    }
    Ok(applied)
}

/// What an earlier provisioning installed in `home`; empty when nothing
/// was recorded or the record cannot be read.
pub fn installed(home: &Path) -> Installed {
    let Ok(target) = resolve(home, INSTALLED_PATH, false) else {
        return Installed::default();
    };
    read_regular(&target)
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Remove the credentials an earlier provisioning recorded in `home`
/// ([`Installed::credentials`]): a whole file, or only Branchyard's
/// variables or keys in it, and forget them. What else the harness or a
/// person put there stays. Files already gone are skipped.
pub fn remove_credentials(home: &Path) -> Result<Applied, ApplyError> {
    let mut record = installed(home);
    if record.credentials.is_empty() {
        return Ok(Applied::default());
    }
    let mut plan = Plan::default();
    for credential in std::mem::take(&mut record.credentials) {
        let path = credential.path().to_owned();
        let present = resolve(home, &path, false)
            .ok()
            .and_then(|target| read_regular(&target).ok().flatten())
            .is_some();
        if !present {
            continue;
        }
        let edit = match credential {
            Credential::File { .. } => Edit::Remove,
            Credential::Dotenv { keys, .. } => Edit::DotenvUnset(keys),
            Credential::Json { keys, .. } => Edit::Json {
                edits: keys.into_iter().map(JsonEdit::Remove).collect(),
                comment_lines: false,
            },
        };
        plan.edit(&path, true, edit);
    }
    let rest = match record == Installed::default() {
        true => Edit::Remove,
        false => Edit::Put(json_text(
            &serde_json::to_value(&record).expect("serializes"),
        )),
    };
    plan.edit(INSTALLED_PATH, false, rest);
    apply(&plan, home)
}

enum Outcome {
    Written,
    Removed,
    Unchanged,
}

fn apply_file(home: &Path, file: &FileEdit) -> Result<Outcome, String> {
    let target = resolve(home, &file.path, true)?;
    let current = read_regular(&target)?;
    let current_mode = fs::symlink_metadata(&target)
        .ok()
        .map(|m| m.permissions().mode() & 0o7777);
    let next = apply_all(&file.edits, current.as_deref())?;
    let Some(content) = next else {
        return match current {
            Some(_) => {
                fs::remove_file(&target).map_err(|e| format!("could not remove it: {e}"))?;
                Ok(Outcome::Removed)
            }
            None => Ok(Outcome::Unchanged),
        };
    };
    let mode = match (file.secret, current_mode) {
        (true, _) => 0o600,
        (false, Some(mode)) => mode,
        (false, None) => 0o644,
    };
    if current.as_deref() == Some(content.as_str()) && current_mode == Some(mode) {
        return Ok(Outcome::Unchanged);
    }
    write_atomically(&target, content.as_bytes(), mode)
        .map_err(|e| format!("could not write it: {e}"))?;
    Ok(Outcome::Written)
}

/// `home` joined with a checked relative path. With `create`, missing
/// directories on the way are created 0700; any existing component that is
/// a symbolic link or not a directory is refused.
fn resolve(home: &Path, relative: &str, create: bool) -> Result<PathBuf, String> {
    let path = Path::new(relative);
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part),
            _ => return Err("is not a plain path inside the home".into()),
        }
    }
    let Some((name, dirs)) = parts.split_last() else {
        return Err("is empty".into());
    };
    let mut current = home.to_path_buf();
    for dir in dirs {
        current.push(dir);
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!(
                    "{} is a symbolic link; not following it",
                    current.display()
                ))
            }
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(format!("{} is not a directory", current.display())),
            Err(e) if e.kind() == io::ErrorKind::NotFound && create => {
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(&current)
                    .map_err(|e| format!("could not create {}: {e}", current.display()))?;
            }
            Err(e) => return Err(format!("{}: {e}", current.display())),
        }
    }
    current.push(name);
    Ok(current)
}

/// The content of a regular file, `None` if there is none. A symbolic link
/// there is treated as absent (it is replaced, not followed); anything else
/// that is not a regular file is refused.
fn read_regular(path: &Path) -> Result<Option<String>, String> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    if meta.file_type().is_symlink() {
        return Ok(None);
    }
    if !meta.is_file() {
        return Err("is not a regular file".into());
    }
    if meta.len() > MAX_FILE {
        return Err(format!("is larger than {MAX_FILE} bytes"));
    }
    let mut text = String::new();
    fs::File::open(path)
        .and_then(|mut f| f.read_to_string(&mut text))
        .map_err(|e| format!("could not read it as UTF-8 text: {e}"))?;
    Ok(Some(text))
}

fn write_atomically(target: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let dir = target.parent().expect("a resolved path has a directory");
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    let temp = dir.join(format!(".{name}.{}.by-tmp", std::process::id()));
    let _ = fs::remove_file(&temp);
    let written = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temp)?;
        file.write_all(bytes)?;
        // The umask may have narrowed the mode; set it exactly.
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.sync_all()?;
        fs::rename(&temp, target)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written
}
