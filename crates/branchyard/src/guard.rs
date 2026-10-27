//! The file guard: a local harness held, by the operating system, to the
//! file scope its permission preset names.
//!
//! A preset decides the tool calls a harness asks Branchyard about. Several
//! harnesses run a shell write without asking (Pi, Antigravity, DeepSeek
//! Harness, Hermes, OpenCode on its defaults), so for them `read-only` and
//! `edit-worktree` decided nothing about files. When a turn runs under one
//! of those presets on this host, the engine starts the harness under
//! Landlock ([`branchyard_runtime::LocalProvider::write_guard`]) with the
//! preset's write scope, and the harness and everything it starts can
//! write only there:
//!
//! - `edit-worktree`: the branch's worktree; its git directory and the
//!   common directory's `objects`, `refs` and `logs`, so a commit the
//!   harness makes lands; the harness's `HOME` (the private home when the
//!   branch is isolated, your own `HOME` otherwise); the branch's private
//!   temporary directory; `/dev`; the scratch areas the branch may reach.
//! - `read-only`: `HOME`, the temporary directory and `/dev`. Not the
//!   worktree, and not the git directory: a read-only turn makes no commit.
//!
//! Reads, execution and the network are not the guard's (the network is
//! the egress policy's, `docs/egress.md`), nor is any one command: inside
//! the scope a harness writes what it likes, and with your own `HOME` in
//! scope it can write your dotfiles. `full`, `--yes`, `--ask` and explicit
//! rules name no scope and run unguarded; a sandbox provider's harness has
//! the sandbox as its boundary. A host without Landlock runs the turn
//! unguarded and records why. What a turn got is the branch's
//! [`Guard`], which `by show`, `by inspect` and `--json` print. See
//! `docs/modes.md#the-file-guard`.
//!
//! A delegating harness needs nothing more: the `by` its shell runs (and
//! `by mcp`) only talks to the engine's broker over a Unix socket, which
//! Landlock's file rights do not cover, and the child's turn runs on a
//! thread of the engine, outside the parent's restriction, under its own
//! guard.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::state::Record;
use crate::{PolicyPreset, Yard};

/// What held a branch's harness to its permission preset's file scope on
/// its last turn: `by show`'s `guard` line and the `guard` field of `by
/// ls --json`, `by show --json` and `by inspect --json`. Serialized as an
/// object tagged by `kind`: `{"kind": "landlock", "preset", "abi",
/// "writable"}` or `{"kind": "none", "reason"}`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Guard {
    /// Landlock held the harness, and every process it started, to writing
    /// beneath `writable` only.
    Landlock {
        /// The preset whose scope it is.
        preset: PolicyPreset,
        /// The kernel's Landlock ABI version.
        abi: u32,
        /// Where it could write, and everything beneath.
        writable: Vec<PathBuf>,
    },
    /// Nothing below the harness held its writes, for this reason.
    #[serde(rename = "none")]
    Unguarded {
        /// Why: no preset with a file scope, a sandbox, or a host without
        /// Landlock (and what it said).
        reason: String,
    },
}

impl Guard {
    /// One line for people, as `by show` prints it.
    pub fn describe(&self) -> String {
        match self {
            Guard::Landlock { preset, abi, .. } => {
                format!(
                    "landlock ({preset}, ABI {abi}): writes only in {}",
                    scope_words(*preset)
                )
            }
            Guard::Unguarded { reason } => format!("none: {reason}"),
        }
    }
}

/// Where a preset's scope lets a harness write, in words.
fn scope_words(preset: PolicyPreset) -> &'static str {
    match preset {
        PolicyPreset::EditWorktree => {
            "its worktree, its git directory, HOME, its temporary directory and /dev"
        }
        _ => "HOME, its temporary directory and /dev; not its worktree",
    }
}

/// Whether this host can hold a local harness's writes, and its Landlock
/// ABI; why not otherwise (`BRANCHYARD_GUARD=off` turns it off). Detected
/// once per process. `by doctor`'s `host.landlock` row.
///
/// # Errors
///
/// The reason the guard is unavailable, as a turn records it.
pub fn support() -> Result<u32, String> {
    branchyard_runtime::LocalProvider::write_guard()
}

/// What `by` says before a local run under `preset` (`isolated`: with a
/// private `HOME`): `Ok` the guard's line, `Err` the note that this host
/// cannot hold it; `None` when the preset names no file scope.
pub fn banner(preset: Option<PolicyPreset>, isolated: bool) -> Option<Result<String, String>> {
    let preset = preset.filter(|p| *p != PolicyPreset::Full)?;
    Some(match support() {
        Ok(_) => {
            let home = match isolated {
                true => "its private HOME",
                false => "your own HOME",
            };
            Ok(match preset {
                PolicyPreset::EditWorktree => format!(
                    "landlock ({preset}): the harness can write only in its worktree, its git \
                     directory, {home}, its temporary directory and /dev"
                ),
                _ => format!(
                    "landlock ({preset}): the harness can write only in {home}, its temporary \
                     directory and /dev, not its worktree"
                ),
            })
        }
        Err(why) => Err(format!(
            "no file guard on this host ({why}): --permissions {preset} decides only the tool \
             calls the harness asks Branchyard about"
        )),
    })
}

/// The turn's guard: the write scope to start a local harness with (`None`:
/// unguarded) and what to record. `home` is the harness's `HOME`; `None`
/// for a harness in a sandbox, which is its own boundary.
pub(crate) fn plan(
    yard: &Yard,
    record: &Record,
    preset: Option<PolicyPreset>,
    home: Option<&Path>,
) -> Result<(Option<Vec<PathBuf>>, Guard), String> {
    let unguarded = |reason: String| Ok((None, Guard::Unguarded { reason }));
    let Some(home) = home else {
        return unguarded(format!(
            "the harness runs in a sandbox ({}), which is its boundary",
            record.info.provider
        ));
    };
    let preset = match preset {
        None => {
            return unguarded(
                "no permission preset names a file scope (--yes, --ask and explicit rules name \
                 none)"
                    .into(),
            )
        }
        Some(PolicyPreset::Full) => {
            return unguarded("the full preset allows every tool and names no file scope".into())
        }
        Some(preset) => preset,
    };
    let abi = match support() {
        Ok(abi) => abi,
        Err(why) => return unguarded(format!("this host cannot hold it: {why}")),
    };
    let writable = scope(yard, record, preset, home)?;
    Ok((
        Some(writable.clone()),
        Guard::Landlock {
            preset,
            abi,
            writable,
        },
    ))
}

/// Where a harness under `preset` may write, as the module documentation
/// lists it.
fn scope(
    yard: &Yard,
    record: &Record,
    preset: PolicyPreset,
    home: &Path,
) -> Result<Vec<PathBuf>, String> {
    let store = yard.store();
    let mut paths = Vec::new();
    if preset == PolicyPreset::EditWorktree {
        let worktree = &record.info.worktree;
        paths.push(worktree.clone());
        let git_dir = crate::git::run(
            worktree,
            &["rev-parse", "--path-format=absolute", "--git-dir"],
        )
        .map(|dir| PathBuf::from(dir.trim()))
        .map_err(|e| format!("could not find its git directory: {e}"))?;
        let common = crate::git::common_dir(worktree)
            .map_err(|e| format!("could not find its repository's git directory: {e}"))?;
        if git_dir != common {
            paths.extend(["objects", "refs", "logs"].map(|dir| common.join(dir)));
        }
        paths.push(git_dir);
        // A local harness writes its scratch areas in place.
        if let Ok(areas) = crate::storage::authorized_scratch(yard, &record.info.name) {
            paths.extend(
                areas
                    .iter()
                    .map(|area| crate::storage::scratch_dir(&store, &area.name)),
            );
        }
    }
    if home.is_absolute() {
        paths.push(home.to_path_buf());
    }
    paths.push(store.tmp(&record.info.name));
    paths.push(PathBuf::from("/dev"));
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_guard_serializes_tagged_by_kind() {
        let held = Guard::Landlock {
            preset: PolicyPreset::EditWorktree,
            abi: 7,
            writable: vec!["/w".into(), "/dev".into()],
        };
        assert_eq!(
            serde_json::to_value(&held).unwrap(),
            serde_json::json!({"kind": "landlock", "preset": "edit-worktree", "abi": 7,
                               "writable": ["/w", "/dev"]})
        );
        let none = Guard::Unguarded {
            reason: "why".into(),
        };
        assert_eq!(
            serde_json::to_value(&none).unwrap(),
            serde_json::json!({"kind": "none", "reason": "why"})
        );
        let back: Guard = serde_json::from_value(serde_json::json!({"kind": "none",
                                                                      "reason": "why"}))
        .unwrap();
        assert_eq!(back, none);
    }

    #[test]
    fn a_guard_describes_itself() {
        let held = Guard::Landlock {
            preset: PolicyPreset::ReadOnly,
            abi: 3,
            writable: Vec::new(),
        };
        assert_eq!(
            held.describe(),
            "landlock (read-only, ABI 3): writes only in HOME, its temporary directory and \
             /dev; not its worktree"
        );
        let none = Guard::Unguarded {
            reason: "the full preset allows every tool and names no file scope".into(),
        };
        assert_eq!(
            none.describe(),
            "none: the full preset allows every tool and names no file scope"
        );
    }

    #[test]
    fn only_a_preset_with_a_scope_has_a_banner() {
        assert_eq!(banner(None, false), None);
        assert_eq!(banner(Some(PolicyPreset::Full), true), None);
        // Hosts differ: either the guard's line or the note, never empty.
        match banner(Some(PolicyPreset::EditWorktree), false) {
            Some(Ok(line)) => {
                assert!(line.starts_with("landlock (edit-worktree)"), "{line}");
                assert!(line.contains("your own HOME"), "{line}");
            }
            Some(Err(note)) => assert!(note.starts_with("no file guard on this host"), "{note}"),
            None => panic!("edit-worktree has a scope"),
        }
        if let Some(Ok(line)) = banner(Some(PolicyPreset::ReadOnly), true) {
            assert!(line.contains("its private HOME"), "{line}");
            assert!(line.contains("not its worktree"), "{line}");
        }
    }
}
