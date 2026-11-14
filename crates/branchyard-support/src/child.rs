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
//!
//! A name is not all a secret has: the same value may be in another
//! variable (a shell that exported a token twice). [`guard`] registers the
//! value, and from then on [`scrub`] goes by it too, whatever the variable
//! is called.

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

static GUARDED: RwLock<Vec<(String, String)>> = RwLock::new(Vec::new());

/// Keep `value` out of every child's environment from now on, under
/// whatever name a variable holds it: [`scrub`] (and so [`command`]) leaves
/// out every variable of this process, and every variable given to the
/// command, whose value holds it. `label` names whose value it is (a model
/// backend), for a message; the value itself is never shown.
pub fn guard(label: &str, value: &str) {
    if value.is_empty() {
        return;
    }
    let mut guarded = GUARDED.write_recovering("guarded values");
    if !guarded.iter().any(|(_, held)| held == value) {
        guarded.push((label.to_owned(), value.to_owned()));
    }
}

/// The label of the guarded value that `value` holds (it is that value, or
/// has it inside), when it holds one ([`guard`]).
pub fn guarded(value: &str) -> Option<String> {
    GUARDED
        .read_recovering("guarded values")
        .iter()
        .find(|(_, held)| value.contains(held.as_str()))
        .map(|(label, _)| label.clone())
}

/// The variables kept out of children, in name order.
pub fn hidden() -> Vec<String> {
    HIDDEN
        .read_recovering("hidden variables")
        .iter()
        .cloned()
        .collect()
}

/// Take the [`hidden`] variables out of `command`'s environment, and every
/// variable whose value holds a guarded value ([`guard`]): one this process
/// has, which the child would inherit, and one the command was given.
pub fn scrub(command: &mut Command) -> &mut Command {
    for name in HIDDEN.read_recovering("hidden variables").iter() {
        command.env_remove(name);
    }
    let guarded = GUARDED.read_recovering("guarded values");
    if guarded.is_empty() {
        return command;
    }
    let holds = |value: &OsStr| {
        let value = value.to_string_lossy();
        guarded
            .iter()
            .any(|(_, held)| value.contains(held.as_str()))
    };
    let mut names: Vec<std::ffi::OsString> = std::env::vars_os()
        .filter(|(_, value)| holds(value))
        .map(|(name, _)| name)
        .collect();
    names.extend(
        command
            .get_envs()
            .filter(|(_, value)| value.is_some_and(holds))
            .map(|(name, _)| name.to_owned()),
    );
    for name in names {
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
