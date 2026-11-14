//! The one constructor of a child process: [`command`] is
//! `std::process::Command::new` without the variables this process holds a
//! secret in for its own use, so no child inherits them.
//!
//! A process that reads a credential from one of its own variables (the
//! model gateway reads an AIPlex token from `AIPLEX_TOKEN`) calls [`hide`]
//! with its name. From then on every child made by [`command`], or passed
//! through [`scrub`], starts without it: a git command and the repository's
//! hooks it runs, a setup command, a check, a server the process starts.
//! The variable stays in this process, which still reads it.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::process::Command;
use std::sync::RwLock;

use crate::RwLockExt as _;

static HIDDEN: RwLock<BTreeSet<String>> = RwLock::new(BTreeSet::new());

/// Keep variable `name` out of every child made from now on.
pub fn hide(name: &str) {
    HIDDEN
        .write_recovering("hidden variables")
        .insert(name.to_owned());
}

/// The variables kept out of children, in name order.
pub fn hidden() -> Vec<String> {
    HIDDEN
        .read_recovering("hidden variables")
        .iter()
        .cloned()
        .collect()
}

/// Take the [`hidden`] variables out of `command`'s environment.
pub fn scrub(command: &mut Command) -> &mut Command {
    for name in HIDDEN.read_recovering("hidden variables").iter() {
        command.env_remove(name);
    }
    command
}

/// `Command::new(program)`, without the [`hidden`] variables.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    scrub(&mut command);
    command
}
