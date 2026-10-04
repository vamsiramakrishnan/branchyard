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

#[cfg(test)]
mod tests {
    //! The executor on a temporary home: what it writes and with which mode,
    //! that a second application changes nothing, and each way a path may
    //! not leave the home. Whole-provisioner cases are in `tests/provision.rs`.
    use std::os::unix::fs::symlink;

    use super::*;

    fn home() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn plan_of(path: &str, secret: bool, edit: Edit) -> Plan {
        let mut plan = Plan::default();
        plan.edit(path, secret, edit);
        plan
    }

    fn put(text: &str) -> Edit {
        Edit::Put(text.into())
    }

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
    }

    fn refused(plan: &Plan, home: &Path) -> ApplyError {
        apply(plan, home).expect_err("must be refused")
    }

    #[test]
    fn a_new_file_is_written_0644_in_new_0700_directories() {
        let h = home();
        let applied = apply(&plan_of("a/b/c.txt", false, put("hi\n")), h.path()).unwrap();
        assert_eq!(applied.written, ["a/b/c.txt"]);
        let file = h.path().join("a/b/c.txt");
        assert_eq!(fs::read_to_string(&file).unwrap(), "hi\n");
        assert_eq!(mode(&file), 0o644);
        assert_eq!(mode(&h.path().join("a")), 0o700);
        assert_eq!(mode(&h.path().join("a/b")), 0o700);
    }

    #[test]
    fn applying_twice_changes_nothing_the_second_time() {
        let h = home();
        let plan = plan_of("x.txt", false, put("same"));
        assert_eq!(apply(&plan, h.path()).unwrap().written, ["x.txt"]);
        let second = apply(&plan, h.path()).unwrap();
        assert_eq!(second.unchanged, ["x.txt"]);
        assert!(second.written.is_empty() && second.removed.is_empty());
    }

    #[test]
    fn a_secret_file_is_0600_and_a_wrong_mode_is_corrected() {
        let h = home();
        let file = h.path().join("key");
        fs::write(&file, "secret").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        // Content is already right, the mode is not: it is rewritten.
        let applied = apply(&plan_of("key", true, put("secret")), h.path()).unwrap();
        assert_eq!(applied.written, ["key"]);
        assert_eq!(mode(&file), 0o600);
        let again = apply(&plan_of("key", true, put("secret")), h.path()).unwrap();
        assert_eq!(again.unchanged, ["key"]);
    }

    #[test]
    fn an_existing_non_secret_file_keeps_its_mode() {
        let h = home();
        let file = h.path().join("run.sh");
        fs::write(&file, "old").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o750)).unwrap();
        apply(&plan_of("run.sh", false, put("new")), h.path()).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "new");
        assert_eq!(mode(&file), 0o750);
    }

    #[test]
    fn no_temporary_file_is_left_behind() {
        let h = home();
        apply(&plan_of("d/f", false, put("1")), h.path()).unwrap();
        let names: Vec<_> = fs::read_dir(h.path().join("d"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["f"]);
    }

    #[test]
    fn remove_deletes_a_file_and_skips_a_missing_one() {
        let h = home();
        fs::write(h.path().join("gone"), "x").unwrap();
        let mut plan = plan_of("gone", false, Edit::Remove);
        plan.edit("never-there", false, Edit::Remove);
        let applied = apply(&plan, h.path()).unwrap();
        assert_eq!(applied.removed, ["gone"]);
        assert_eq!(applied.unchanged, ["never-there"]);
        assert!(!h.path().join("gone").exists());
    }

    #[test]
    fn a_json_edit_merges_into_what_is_there_and_keeps_the_rest() {
        let h = home();
        fs::write(h.path().join("c.json"), "{\"keep\": 1}\n").unwrap();
        let edit = Edit::Json {
            edits: vec![JsonEdit::Set(
                vec!["a".into(), "b".into()],
                serde_json::json!(2),
            )],
            comment_lines: false,
        };
        apply(&plan_of("c.json", false, edit), h.path()).unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(h.path().join("c.json")).unwrap()).unwrap();
        assert_eq!(value, serde_json::json!({"keep": 1, "a": {"b": 2}}));
    }

    #[test]
    fn paths_that_leave_the_home_are_refused() {
        let h = home();
        for bad in ["../escape", "/etc/passwd", "a/../../b", "./x", ""] {
            let err = refused(&plan_of(bad, false, put("x")), h.path());
            assert_eq!(err.path, bad);
            assert!(!err.reason.is_empty(), "{bad}");
        }
        assert!(!h.path().parent().unwrap().join("escape").exists());
    }

    #[test]
    fn a_symbolic_link_directory_on_the_way_is_not_followed() {
        let h = home();
        let outside = home();
        symlink(outside.path(), h.path().join("link")).unwrap();
        let err = refused(&plan_of("link/secret", true, put("s3cret")), h.path());
        assert!(err.reason.contains("symbolic link"), "{err}");
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_file_where_a_directory_is_needed_is_refused() {
        let h = home();
        fs::write(h.path().join("dir"), "i am a file").unwrap();
        let err = refused(&plan_of("dir/x", false, put("x")), h.path());
        assert!(err.reason.contains("is not a directory"), "{err}");
    }

    #[test]
    fn a_symbolic_link_at_the_target_is_replaced_not_followed() {
        let h = home();
        let outside = home();
        let victim = outside.path().join("victim");
        fs::write(&victim, "untouched").unwrap();
        symlink(&victim, h.path().join("cred")).unwrap();
        let applied = apply(&plan_of("cred", true, put("s3cret")), h.path()).unwrap();
        assert_eq!(applied.written, ["cred"]);
        assert_eq!(fs::read_to_string(&victim).unwrap(), "untouched");
        let target = h.path().join("cred");
        assert!(!fs::symlink_metadata(&target)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_to_string(&target).unwrap(), "s3cret");
    }

    #[test]
    fn a_directory_at_the_target_and_an_unreadable_file_are_refused() {
        let h = home();
        fs::create_dir(h.path().join("d")).unwrap();
        let err = refused(&plan_of("d", false, put("x")), h.path());
        assert!(err.reason.contains("not a regular file"), "{err}");

        fs::write(h.path().join("bin"), [0xff, 0xfe, 0x00]).unwrap();
        let err = refused(
            &plan_of("bin", false, Edit::DotenvUnset(vec!["A".into()])),
            h.path(),
        );
        assert!(err.reason.contains("UTF-8"), "{err}");
    }

    #[test]
    fn a_file_over_the_size_limit_is_refused_without_reading_it() {
        let h = home();
        let big = fs::File::create(h.path().join("big")).unwrap();
        big.set_len(MAX_FILE + 1).unwrap();
        let err = refused(&plan_of("big", false, put("x")), h.path());
        assert!(err.reason.contains("is larger than"), "{err}");
    }

    #[test]
    fn the_home_must_be_an_existing_directory() {
        let h = home();
        let plan = plan_of("x", false, put("x"));
        let missing = refused(&plan, &h.path().join("nope"));
        assert!(missing.reason.contains("not usable"), "{missing}");
        fs::write(h.path().join("file"), "").unwrap();
        let file = refused(&plan, &h.path().join("file"));
        assert!(file.reason.contains("not a directory"), "{file}");
    }

    #[test]
    fn an_error_names_the_file_and_the_problem_but_not_the_content() {
        let h = home();
        fs::create_dir(h.path().join("d")).unwrap();
        let err = refused(&plan_of("d", true, put("TOP-SECRET-VALUE")), h.path());
        let text = err.to_string();
        assert!(text.starts_with("d: "), "{text}");
        assert!(!text.contains("TOP-SECRET-VALUE"));
    }

    #[test]
    fn the_first_failing_file_stops_the_plan_after_earlier_files_were_written() {
        let h = home();
        let mut plan = plan_of("first", false, put("1"));
        plan.edit("../second", false, put("2"));
        plan.edit("third", false, put("3"));
        let err = refused(&plan, h.path());
        assert_eq!(err.path, "../second");
        assert!(h.path().join("first").exists());
        assert!(!h.path().join("third").exists());
    }

    #[test]
    fn installed_is_empty_without_a_record_or_with_a_broken_one() {
        let h = home();
        assert_eq!(installed(h.path()), Installed::default());
        fs::create_dir(h.path().join(".branchyard")).unwrap();
        fs::write(h.path().join(INSTALLED_PATH), "not json").unwrap();
        assert_eq!(installed(h.path()), Installed::default());
    }

    fn record(home: &Path, record: &Installed) {
        let text = json_text(&serde_json::to_value(record).unwrap());
        apply(&plan_of(INSTALLED_PATH, false, Edit::Put(text)), home).unwrap();
    }

    #[test]
    fn the_installed_record_round_trips() {
        let h = home();
        let want = Installed {
            mcp_servers: vec!["one".into()],
            credentials: vec![Credential::File { path: "k".into() }],
        };
        record(h.path(), &want);
        assert_eq!(installed(h.path()), want);
    }

    #[test]
    fn removing_credentials_with_none_recorded_does_nothing() {
        let h = home();
        assert_eq!(remove_credentials(h.path()).unwrap(), Applied::default());
        assert!(!h.path().join(INSTALLED_PATH).exists());
    }

    #[test]
    fn removing_credentials_takes_only_branchyards_and_forgets_them() {
        let h = home();
        fs::write(h.path().join("whole"), "token").unwrap();
        fs::write(h.path().join(".env"), "# mine\nKEEP=1\nBY_KEY=secret\n").unwrap();
        fs::write(
            h.path().join("auth.json"),
            "{\"mine\": true, \"token\": \"t\"}\n",
        )
        .unwrap();
        record(
            h.path(),
            &Installed {
                mcp_servers: vec!["srv".into()],
                credentials: vec![
                    Credential::File {
                        path: "whole".into(),
                    },
                    Credential::Dotenv {
                        path: ".env".into(),
                        keys: vec!["BY_KEY".into()],
                    },
                    Credential::Json {
                        path: "auth.json".into(),
                        keys: vec![vec!["token".into()]],
                    },
                    Credential::File {
                        path: "already-gone".into(),
                    },
                ],
            },
        );
        let applied = remove_credentials(h.path()).unwrap();
        assert_eq!(applied.removed, ["whole"]);
        assert!(!h.path().join("whole").exists());
        assert_eq!(
            fs::read_to_string(h.path().join(".env")).unwrap(),
            "# mine\nKEEP=1\n"
        );
        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(h.path().join("auth.json")).unwrap()).unwrap();
        assert_eq!(json, serde_json::json!({"mine": true}));
        // The MCP record stays; the credentials are forgotten.
        let left = installed(h.path());
        assert_eq!(left.mcp_servers, ["srv"]);
        assert!(left.credentials.is_empty());
        // And a second removal has nothing to do.
        assert_eq!(remove_credentials(h.path()).unwrap(), Applied::default());
    }

    #[test]
    fn removing_the_last_record_removes_the_record_file() {
        let h = home();
        fs::write(h.path().join("whole"), "token").unwrap();
        record(
            h.path(),
            &Installed {
                mcp_servers: vec![],
                credentials: vec![Credential::File {
                    path: "whole".into(),
                }],
            },
        );
        remove_credentials(h.path()).unwrap();
        assert!(!h.path().join(INSTALLED_PATH).exists());
    }
}
