//! The check before `by run`, `by fan` and `by send` start a harness here:
//! is it installed, and is it logged out for certain? A branch created for
//! a harness that cannot start only fails after the fact, sometimes after
//! the harness has retried a 401 for half a minute, so the refusal comes
//! first and ends with the command to run. See docs/harness-lifecycle.md.
//!
//! It asks the inventory `by harnesses` shows (the cached one when it is
//! fresh, else a detection of just the harnesses named) and acts on what
//! that knows:
//!
//! | The inventory says | Here |
//! |---|---|
//! | not installed | refuse, with `by harnesses install ID`, or the catalog's command when the install policy would refuse it |
//! | found, not on `PATH` | refuse, with the directory to add |
//! | found, older than the driver can follow (Pi before 0.80.4 sends no `agent_settled`, so a run never ends) | refuse, with the command that installs a newer one; `--no-preflight` runs anyway |
//! | logged out, verified by the harness's own status command | refuse, with the login hint; `--no-preflight` runs anyway |
//! | logged out, but the run is isolated and its secrets (`--secret`, `[secrets]`) name the harness's key variable | one note, and go on: the private home gets a login the host's status command cannot see |
//! | logged out, likely (a guess from credential files) | one note, and go on |
//! | logged in, or unknown | go on |
//!
//! A key `by harnesses login ID --api-key` stored counts as a login here as
//! it does in the listing.
//!
//! Nothing here is checked for a run that names its own executable
//! (`--command`, which a branch keeps for its later sends), a routed run (the router asks the same inventory), a run
//! in a sandbox or on a recipe's machine (the image provides the harness),
//! a harness running on a branch, or `--no-preflight`.

use branchyard::inventory::{
    self, Evidence, InstallAction, InstallMode, Inventory, LoginState, Permission,
};
use branchyard_controls::catalog;
use branchyard_harness::profiles;

use crate::args::TaskArgs;
use crate::commands::{Env, Failure, Outcome};

/// Refuse, before any branch is created, a harness among `requested`
/// (harness or profile IDs, as `--harness` takes them) that is not
/// installed or is verified logged out. A harness the catalog does not
/// know, or that no profile drives, is left to the engine, which says so.
/// `by_default` says the harness is the built-in default, not one asked
/// for, which changes what a refusal suggests.
pub(crate) fn check(
    env: &Env,
    task: &TaskArgs,
    requested: &[String],
    by_default: bool,
) -> Result<(), Failure> {
    if skipped(task) {
        return Ok(());
    }
    let wanted = detectable(requested);
    if wanted.is_empty() {
        return Ok(());
    }
    let found = match crate::harness_cmd::detected_only(&wanted) {
        Ok(found) => found,
        Err(why) => {
            eprintln!("by: note: could not look for {}: {why}", wanted.join(", "));
            return Ok(());
        }
    };
    for id in &wanted {
        decide(env, task, &found, id, by_default)?;
    }
    Ok(())
}

/// The key variable of `id` that this run places in the harness's private
/// home (`--secret`, or `[secrets]` applied to an isolated run): a login
/// the host's status command cannot see. `None` without a private home,
/// since the key would not reach the harness.
fn provisioned_key(task: &TaskArgs, id: &str) -> Option<String> {
    if !task.isolated {
        return None;
    }
    let names = inventory::key_variables(id);
    task.provision
        .as_ref()?
        .secrets
        .iter()
        .map(|secret| secret.name.clone())
        .find(|name| names.contains(name))
}

/// A pre-flight refusal, as `--json` reports an unavailable harness:
/// `{"error": {"kind": "harness_unavailable", "message"}}` on stdout, exit 1.
/// Any other failure is returned as it is.
pub(crate) fn refused(json: bool, failure: Failure) -> Outcome {
    match (json, failure) {
        (true, Failure::Message(message)) => {
            let value = serde_json::json!({
                "error": {"kind": "harness_unavailable", "message": message},
            });
            crate::commands::print(&crate::json::text(&value))?;
            Err(Failure::Reported)
        }
        (_, other) => Err(other),
    }
}

/// Whether this run is not the pre-flight's business.
fn skipped(task: &TaskArgs) -> bool {
    task.no_preflight
        || task.command.is_some()
        || crate::fleet_cmd::is_routed(task)
        || task.sandbox.is_some()
        || task.substrate.is_some()
        || task.recipe.is_some()
        || crate::workspace_cmd::in_harness().is_some()
}

/// The catalog IDs whose executables the requested profiles launch, once
/// each. A profile whose executable the catalog does not list (an ACP
/// adapter such as `claude-agent-acp`) is left out: what detection finds
/// would not be what runs.
fn detectable(requested: &[String]) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for name in requested {
        let Some(profile) = profiles::default_for(name).or_else(|| profiles::by_id(name)) else {
            continue;
        };
        let Some(entry) = catalog::harness(profile.harness) else {
            continue;
        };
        let launches = profile.command.first().copied().unwrap_or_default();
        if entry.binaries.iter().any(|b| b == launches) && !ids.contains(&entry.id) {
            ids.push(entry.id.clone());
        }
    }
    ids
}

/// What the inventory says about `id`: refuse, note, or nothing.
fn decide(
    env: &Env,
    task: &TaskArgs,
    found: &Inventory,
    id: &str,
    by_default: bool,
) -> Result<(), Failure> {
    let Some(state) = found.get(id) else {
        return match found.checked(id) {
            true => Err(Failure::Message(not_installed(env, found, id, by_default))),
            false => Ok(()),
        };
    };
    // A release the driver cannot follow to the end of a turn: the harness
    // would do the work and the run would never finish.
    if state.on_path {
        if let Some((why, fix)) = state.too_old() {
            return Err(Failure::Message(format!(
                "{id} {why}.\nNothing was started.\nNext: {fix}\n(--no-preflight starts it \
                 anyway.)"
            )));
        }
    }
    let logged_out = state.login.state == LoginState::LoggedOut;
    let certain = state.login.evidence == Evidence::Verified;
    // `ready` words each case, and its first two are the ones that matter
    // here; its usage-limit case is `[usage] guard`'s business.
    let Err(why) = state.ready() else {
        return Ok(());
    };
    if !state.on_path {
        return Err(Failure::Message(why));
    }
    if logged_out {
        if let Some(var) = provisioned_key(task, id) {
            eprintln!(
                "by: note: {id} is logged out here; the isolated run gets {var} from its \
                 secrets. Going on."
            );
            return Ok(());
        }
    }
    if logged_out && certain {
        return Err(Failure::Message(format!(
            "{why}\nNothing was started. If the login is fine (Codex with a model provider of its \
             own, say), pass --no-preflight."
        )));
    }
    if logged_out {
        eprintln!("by: note: {why}. Going on, since that is a guess from files and variables.");
    }
    Ok(())
}

/// `id` is not installed: what to run, under the install policy.
fn not_installed(env: &Env, found: &Inventory, id: &str, by_default: bool) -> String {
    let mut text = format!("{id} is not installed on this machine.");
    let interactive = env.stdin_tty && env.stderr_tty;
    let policy = crate::harness_cmd::policy(env).ok();
    let permitted = policy
        .as_ref()
        .map_or(Permission::Ask, |p| p.permits(id, false, interactive));
    // An allow list that leaves `id` out is what refuses it, whatever the mode.
    let left_out = policy
        .as_ref()
        .is_some_and(|p| !p.allow.is_empty() && !p.allow.iter().any(|a| a == id));
    match (permitted, &policy) {
        (Permission::Run | Permission::Ask, _) => {
            text.push_str(&format!("\nNext: by harnesses install {id}"))
        }
        // Allowed, but there is no terminal to ask on.
        (Permission::Refused(_), Some(p)) if p.mode == InstallMode::Ask && !left_out => {
            text.push_str(&format!("\nNext: by harnesses install {id} --yes"))
        }
        (Permission::Refused(_), _) => {
            let command = inventory::plan(id, InstallAction::Install, None, &found.tools)
                .ok()
                .map(|p| p.command)
                .or_else(|| catalog::harness(id).and_then(|e| e.install.first().cloned()));
            let Some(command) = command else {
                text.push_str(" The catalog records no install command for it.");
                return text;
            };
            text.push_str(&format!("\nNext: {command}"));
            let file = crate::setup_io::user_file();
            text.push_str(&match left_out {
                true => format!(
                    "\n(To let `by harnesses install {id}` do it, add \"{id}\" to allow under \
                     [harnesses] in {}.)",
                    file.display()
                ),
                false => format!(
                    "\n(To let `by harnesses install {id}` do it, set install = \"ask\" under \
                     [harnesses] in {}.)",
                    file.display()
                ),
            });
        }
    }
    if by_default {
        text.push_str(
            "\n`by run` uses claude-code unless --harness (or [defaults] harness) says otherwise; \
             `by harnesses` lists what is installed.",
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn harness_and_profile_ids_name_the_catalog_harness_they_launch_once() {
        assert_eq!(
            detectable(&names(&["codex", "codex-app-server", "claude-code"])),
            ["codex", "claude-code"]
        );
    }

    #[test]
    fn cursor_is_found_by_the_executable_its_profile_runs() {
        // `cursor-acp` runs `agent`, which the catalog lists for Cursor.
        assert_eq!(detectable(&names(&["cursor-acp"])), ["cursor"]);
    }

    #[test]
    fn an_adapter_the_catalog_does_not_list_is_not_checked() {
        // claude-agent-acp is not `claude`: finding Claude Code says nothing.
        assert!(detectable(&names(&["claude-code-acp", "codex-acp"])).is_empty());
    }

    #[test]
    fn a_harness_nothing_drives_is_left_to_the_engine() {
        assert!(detectable(&names(&["droid", "nope"])).is_empty());
    }
}
