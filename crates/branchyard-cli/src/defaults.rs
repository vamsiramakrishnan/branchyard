//! Configuration defaults under the flags. [`apply`] is the one call site:
//! it fills what the command line and the `BRANCHYARD_*` variables left
//! unset from the merged user and project files (`branchyard.toml`). The
//! per-field rules live in [`apply_task`], [`apply_globals`] and
//! [`serve_args`].
//!
//! Precedence is flags, then variables, then the project file, then the
//! user file. clap reads the variables behind the global options into
//! [`Globals`] itself (`env = ...`), so a variable already counts as given
//! when these run; no option these fill has a clap default or variable of
//! its own, so `None` (or `Permissions::Unset`, or a bool left false)
//! means the command line said nothing. `by serve`'s `--config` is
//! detected by the server's own parser (`value_source`), which knows
//! `-c FILE` and `--config=FILE` alike.
//!
//! - New branches (`run`, `fan`) take every default: harness, model,
//!   effort, auth, limits, check, permissions, isolation, provider,
//!   instructions, MCP servers, and secrets when the branch has a private
//!   home (isolated or sandboxed).
//! - `send`, `fork`, `reincarnate` and `spawn` take only `permissions`: the
//!   rest would override what the branch or its seat already has.
//! - `serve` and `worker` take `serve.config` as `--config` ([`apply_serve`]).
//! - Nothing is read inside a harness running on a branch
//!   (`BRANCHYARD_BRANCH` set): its `by` acts for that branch, whose
//!   options the engine already set.

use std::path::Path;
use std::time::Duration;

use branchyard_setup::config::{Effective, PermissionsMode, ProjectConfig, ProviderKind};

use crate::args::{Command, Globals, Permissions, SandboxArgs, TaskArgs};

/// Which defaults a command takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// A new top-level branch: every default.
    NewBranch,
    /// An existing branch, a fork or a delegated child: `permissions` only.
    Continue,
}

/// Fill `globals` and `command` from the configuration files under `cwd`.
/// `env` reads variables (only `BRANCHYARD_BRANCH` here).
pub fn apply(
    cwd: &Path,
    env: &dyn Fn(&str) -> Option<String>,
    mut globals: Globals,
    mut command: Command,
) -> Result<(Globals, Command), String> {
    if matches!(
        command,
        Command::Init { .. }
            | Command::Config { .. }
            | Command::Mcp { .. }
            | Command::Completions { .. }
            | Command::Man
    ) {
        return Ok((globals, command));
    }
    let Some(config) = load(cwd, env)? else {
        return Ok((globals, command));
    };
    match &mut command {
        Command::Serve { args } | Command::Worker { args } => {
            *args = serve_args(&config, std::mem::take(args));
            return Ok((globals, command));
        }
        Command::Run { task, .. } => apply_task(&config, task, Scope::NewBranch)?,
        Command::Fan {
            task, harnesses, ..
        } => {
            let explicit = task.auto;
            apply_task(&config, task, Scope::NewBranch)?;
            // fan's harnesses are its --harness list, which routing replaces.
            task.harness = None;
            if harnesses.is_some() {
                if explicit {
                    return Err(
                        "--auto routes through the [fleet] table; it takes no --harness".into(),
                    );
                }
                task.implied_auto = false;
            }
        }
        Command::Send { task, .. } => apply_task(&config, task, Scope::Continue)?,
        Command::Fork { task, .. } => apply_task(&config, task, Scope::Continue)?,
        Command::Reincarnate { task, .. } => apply_task(&config, task, Scope::Continue)?,
        Command::Spawn { spawn, .. } => apply_task(&config, &mut spawn.task, Scope::Continue)?,
        _ => {}
    }
    apply_globals(&config, &mut globals);
    Ok((globals, command))
}

/// `by serve` and `by worker`, which `main` hands to the server's parser
/// before `by`'s own runs: the server's arguments with `[serve] config`.
pub fn apply_serve(
    cwd: &Path,
    env: &dyn Fn(&str) -> Option<String>,
    args: Vec<String>,
) -> Result<Vec<String>, String> {
    Ok(match load(cwd, env)? {
        Some(config) => serve_args(&config, args),
        None => args,
    })
}

/// The merged user and project files, or `None` when there are none or
/// `by` runs inside a harness's branch.
fn load(cwd: &Path, env: &dyn Fn(&str) -> Option<String>) -> Result<Option<ProjectConfig>, String> {
    if env("BRANCHYARD_BRANCH").is_some_and(|v| !v.is_empty()) {
        return Ok(None);
    }
    let located = crate::setup_io::locate(cwd);
    if !located.project_exists && !located.user.is_file() {
        return Ok(None);
    }
    // Files only: the variables are already in `globals`, and win.
    let Effective { config, .. } = crate::setup_io::load(cwd, None)
        .map_err(|e| format!("{e}\n(fix it, or check it with `by config validate`)"))?;
    Ok(Some(config))
}

/// The merged configuration files under `cwd`, for commands that read a
/// section of their own (`[usage]`, `[trackers]`): `None` without files, or
/// inside a harness's branch.
pub fn config_at(
    cwd: &Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<ProjectConfig>, String> {
    load(cwd, env)
}

/// `[notify]`, and `[remote]` for what `--remote`, `--token-file`,
/// `--ca-file`, `--repo` and their variables left unset. The rest of `[remote]` applies only
/// when its `url` is the server in use.
pub fn apply_globals(config: &ProjectConfig, globals: &mut Globals) {
    globals.notify = config.notify.clone();
    let remote = &config.remote;
    let same_server = match (&globals.remote, &remote.url) {
        (None, Some(_)) => {
            globals.remote = remote.url.clone();
            true
        }
        (Some(given), Some(url)) => given.trim_end_matches('/') == url.trim_end_matches('/'),
        (_, None) => globals.remote.is_none(),
    };
    if !same_server || globals.remote.is_none() {
        return;
    }
    if globals.token_file.is_none() {
        globals.token_file = remote.token_file.clone();
    }
    if globals.ca_file.is_none() {
        globals.ca_file = remote.ca_file.clone();
    }
    if globals.repo.is_none() {
        globals.repo = remote.repo.clone();
    }
}

/// `by serve` and `by worker`: `serve.config` as `--config` unless the
/// command line gives one, or needs none (help, `token new`); see
/// [`branchyard_server::cli::names_config`].
pub fn serve_args(config: &ProjectConfig, args: Vec<String>) -> Vec<String> {
    let Some(path) = &config.serve.config else {
        return args;
    };
    if branchyard_server::cli::names_config(&args) {
        return args;
    }
    // `token list`, `token revoke`, `token new --link`: the subcommand's
    // own `--config`, after it.
    if args.first().is_some_and(|a| a == "token") {
        let mut with = args;
        with.extend(["--config".to_owned(), path.clone()]);
        return with;
    }
    let mut with = vec!["--config".to_owned(), path.clone()];
    with.extend(args);
    with
}

/// Fill what the flags left unset in `task`.
pub fn apply_task(config: &ProjectConfig, task: &mut TaskArgs, scope: Scope) -> Result<(), String> {
    let d = &config.defaults;
    if task.permissions == Permissions::Unset {
        task.permissions = match d.permissions {
            Some(PermissionsMode::Ask) => Permissions::Ask,
            Some(PermissionsMode::Yes) => Permissions::Yes,
            None => Permissions::Unset,
        };
    }
    if scope == Scope::Continue {
        return Ok(());
    }
    if !config.fleet.is_empty() {
        task.fleet = Some(fleet(config)?);
        // A [fleet] routes a task that names no harness (docs/fleet.md).
        if task.harness.is_none() && !task.auto {
            task.implied_auto = true;
        }
    }
    if task.harness.is_none() && !task.auto && !task.implied_auto {
        task.harness = d.harness.clone();
    }
    if task.check.is_none() {
        if let Some(check) = &d.check {
            task.check = Some(
                branchyard_setup::config::split_words(check)
                    .map_err(|e| format!("defaults.check: {e}"))?,
            );
        }
    }
    task.budget_usd = task.budget_usd.or(d.budget_usd);
    task.max_turns = task.max_turns.or(d.max_turns);
    if task.max_duration.is_none() {
        task.max_duration = d
            .max_minutes
            .and_then(|minutes| Duration::try_from_secs_f64(minutes * 60.0).ok());
    }
    if d.isolated == Some(true) {
        task.isolated = true;
    }
    let chose_provider =
        task.sandbox.is_some() || task.substrate.is_some() || task.recipe.is_some() || task.local;
    if let (false, Some(ProviderKind::Recipe), Some(name)) = (chose_provider, d.provider, &d.recipe)
    {
        task.recipe = Some(crate::args::RecipeArgs {
            name: name.clone(),
            ..crate::args::RecipeArgs::default()
        });
    }
    if !chose_provider && d.provider == Some(ProviderKind::Microsandbox) {
        if let Some(sandbox) = &config.microsandbox {
            task.sandbox = Some(SandboxArgs {
                image: sandbox.image.clone(),
                cpus: sandbox.cpus,
                memory_mib: sandbox.memory_mib,
                pass_env: sandbox.pass_env.clone(),
                lifecycle: crate::args::LifecycleArgs {
                    keep: sandbox.keep.as_deref().map(|keep| match keep {
                        "pause" => branchyard::SandboxKeep::Pause,
                        _ => branchyard::SandboxKeep::Destroy,
                    }),
                    snapshots: sandbox.snapshots,
                    max_paused: sandbox.max_paused,
                },
                live_branch: sandbox.live_branch.unwrap_or(false),
            });
        }
    }
    if task.instructions.is_none() {
        task.instructions = d.instructions.clone();
    }
    // Provisioning: fill each unset part; add servers and secrets the
    // flags did not name.
    let private_home = task.isolated
        || task.sandbox.is_some()
        || task.substrate.is_some()
        || task.recipe.is_some();
    let secrets = match private_home {
        true => config.secret_sources().map_err(|e| e.to_string())?,
        false => Vec::new(),
    };
    let mcp: Vec<branchyard::McpServerSpec> = config
        .mcp
        .iter()
        .map(|(name, command)| branchyard::McpServerSpec::parse(&format!("{name}={command}")))
        .collect::<Result<_, _>>()
        .map_err(|e| format!("mcp: {e}"))?;
    let effort = match &d.effort {
        Some(text) => {
            Some(branchyard::Effort::parse(text).map_err(|e| format!("defaults.effort: {e}"))?)
        }
        None => None,
    };
    // Default grants, only where they can be placed and only when the
    // flags gave none.
    let grants = match private_home {
        true => config
            .connectors
            .grant_entries()
            .map_err(|e| e.to_string())?,
        false => Vec::new(),
    };
    let flag_grants = task
        .provision
        .as_ref()
        .is_some_and(|p| !p.connectors.is_empty());
    let wanted = d.model.is_some()
        || effort.is_some()
        || d.auth.is_some()
        || !secrets.is_empty()
        || !mcp.is_empty()
        || (!grants.is_empty() && !flag_grants);
    if wanted {
        let spec = task.provision.get_or_insert_with(Default::default);
        if spec.model.is_none() {
            spec.model = d.model.clone();
        }
        if spec.effort.is_none() {
            spec.effort = effort;
        }
        if spec.auth.is_none() {
            spec.auth = d.auth.clone();
        }
        for secret in secrets {
            if !spec.secrets.iter().any(|s| s.name == secret.name) {
                spec.secrets.push(secret);
            }
        }
        for server in mcp {
            if !spec.mcp_servers.iter().any(|s| s.name == server.name) {
                spec.mcp_servers.push(server);
            }
        }
        if spec.connectors.is_empty() {
            spec.connectors = grants;
        }
    }
    Ok(())
}

/// The configuration's `[fleet]` as the SDK takes it.
pub fn fleet(config: &ProjectConfig) -> Result<branchyard::Fleet, String> {
    let words = |key: &str, line: &Option<String>| -> Result<Option<Vec<String>>, String> {
        line.as_deref()
            .map(branchyard_setup::config::split_words)
            .transpose()
            .map_err(|e| format!("{key}: {e}"))
    };
    let effort = |key: &str, text: &Option<String>| -> Result<Option<branchyard::Effort>, String> {
        text.as_deref()
            .map(branchyard::Effort::parse)
            .transpose()
            .map_err(|e| format!("{key}: {e}"))
    };
    let mut entries = std::collections::BTreeMap::new();
    for (kind, entry) in &config.fleet {
        let key = format!("fleet.{kind}");
        let mut candidates = Vec::new();
        for (index, c) in entry.candidates.iter().enumerate() {
            let at = format!("{key}.candidates[{index}]");
            candidates.push(branchyard::FleetCandidate {
                harness: c.harness.clone(),
                model: c.model.clone(),
                effort: effort(&at, &c.effort)?,
                command: words(&at, &c.command)?,
            });
        }
        let judge = match &entry.judge {
            Some(j) => Some(branchyard::JudgeSpec {
                harness: j.harness.clone(),
                model: j.model.clone(),
                effort: effort(&format!("{key}.judge"), &j.effort)?,
                command: words(&format!("{key}.judge"), &j.command)?,
                rubric: j.rubric.clone(),
            }),
            None => None,
        };
        entries.insert(
            kind.clone(),
            branchyard::FleetEntry {
                candidates,
                attempts: entry.attempts.unwrap_or(1),
                budget: branchyard::Budget {
                    max_usd: entry.budget_usd,
                    max_turns: entry.max_turns,
                    max_duration: entry
                        .max_minutes
                        .and_then(|m| Duration::try_from_secs_f64(m * 60.0).ok()),
                    ..branchyard::Budget::default()
                },
                judge,
                failover: entry.failover.unwrap_or(false),
                exploration: entry.exploration.unwrap_or(branchyard::DEFAULT_EXPLORATION),
                environment: entry.environment.clone(),
                connectors: entry.connectors.clone(),
                plan: entry.plan.unwrap_or(false),
                goal_judge: match &entry.goal_judge {
                    Some(j) => Some(branchyard::JudgeSpec {
                        harness: j.harness.clone(),
                        model: j.model.clone(),
                        effort: effort(&format!("{key}.goal_judge"), &j.effort)?,
                        command: words(&format!("{key}.goal_judge"), &j.command)?,
                        rubric: j.rubric.clone(),
                    }),
                    None => None,
                },
            },
        );
    }
    Ok(branchyard::Fleet { entries })
}

/// The `[fleet]` of the files under `cwd`, for commands that take no task
/// options (`by judge`, `by fleet`); `None` without one, or inside a
/// harness.
pub fn fleet_at(
    cwd: &Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<branchyard::Fleet>, String> {
    match load(cwd, env)? {
        Some(config) if !config.fleet.is_empty() => fleet(&config).map(Some),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> ProjectConfig {
        branchyard_setup::config::parse(text).unwrap()
    }

    const FILE: &str = r#"
[defaults]
harness = "codex"
model = "large"
effort = "high"
budget_usd = 5
max_turns = 9
max_minutes = 2
permissions = "ask"
isolated = true
check = "cargo test -p x"
[secrets]
OPENAI_API_KEY = "MY_OPENAI"
[mcp]
docs = "/bin/docs --stdio"
[remote]
url = "http://127.0.0.1:9"
token_file = "/t/token"
[serve]
config = "/srv/server.json"
"#;

    #[test]
    fn a_new_branch_takes_every_default_the_flags_left_unset() {
        let config = config(FILE);
        let mut task = TaskArgs {
            budget_usd: Some(1.0),
            ..TaskArgs::default()
        };
        apply_task(&config, &mut task, Scope::NewBranch).unwrap();
        assert_eq!(task.harness.as_deref(), Some("codex"));
        assert_eq!(task.budget_usd, Some(1.0), "the flag wins");
        assert_eq!(task.max_turns, Some(9));
        assert_eq!(task.max_duration, Some(Duration::from_secs(120)));
        assert_eq!(task.permissions, Permissions::Ask);
        assert!(task.isolated);
        assert_eq!(
            task.check.as_deref(),
            Some(&["cargo".to_owned(), "test".into(), "-p".into(), "x".into()][..])
        );
        let spec = task.provision.unwrap();
        assert_eq!(spec.model.as_deref(), Some("large"));
        assert_eq!(spec.secrets.len(), 1);
        assert_eq!(spec.secrets[0].name, "OPENAI_API_KEY");
        assert_eq!(spec.mcp_servers[0].name, "docs");
    }

    #[test]
    fn a_fleet_routes_new_branches_that_name_no_harness() {
        let config = config(
            r#"
[defaults]
harness = "codex"
[fleet.default]
candidates = [{ harness = "codex", model = "large", effort = "high", command = "/x/codex --y" }]
attempts = 3
budget_usd = 2
max_minutes = 1
failover = true
judge = { harness = "claude-code", rubric = "short" }
environment = "rust"
connectors = ["github"]
"#,
        );
        let mut task = TaskArgs::default();
        apply_task(&config, &mut task, Scope::NewBranch).unwrap();
        assert!(task.implied_auto && !task.auto);
        assert_eq!(task.harness, None, "the router picks, not [defaults]");
        let fleet = task.fleet.unwrap();
        let entry = &fleet.entries["default"];
        assert_eq!(entry.attempts, 3);
        assert_eq!(entry.budget.max_usd, Some(2.0));
        assert_eq!(entry.budget.max_duration, Some(Duration::from_secs(60)));
        assert!(entry.failover);
        assert_eq!(entry.exploration, branchyard::DEFAULT_EXPLORATION);
        assert_eq!(entry.candidates[0].effort, Some(branchyard::Effort::High));
        assert_eq!(
            entry.candidates[0].command.as_deref(),
            Some(&["/x/codex".to_owned(), "--y".into()][..])
        );
        assert_eq!(
            entry.judge.as_ref().unwrap().rubric.as_deref(),
            Some("short")
        );
        assert_eq!(entry.connectors, ["github"]);
        // A named harness is not routed.
        let mut named = TaskArgs {
            harness: Some("gemini-cli".into()),
            ..TaskArgs::default()
        };
        apply_task(&config, &mut named, Scope::NewBranch).unwrap();
        assert!(!named.implied_auto);
        // The table's keys are the SDK's kinds and `default`.
        let kinds: Vec<&str> = branchyard::TaskKind::ALL
            .iter()
            .map(|k| k.as_str())
            .chain(["default"])
            .collect();
        assert_eq!(branchyard_setup::config::FLEET_KEYS, kinds.as_slice());
    }

    #[test]
    fn default_grants_go_only_to_a_new_private_branch_without_its_own() {
        let config = config(
            "[connectors]\ngateway = \"http://127.0.0.1:8931/mcp\"\ngrants = [\"github:read\"]\n",
        );
        let grants = |task: &TaskArgs| -> Vec<String> {
            task.provision
                .as_ref()
                .map(|p| p.connectors.iter().map(|g| g.to_string()).collect())
                .unwrap_or_default()
        };
        let mut isolated = TaskArgs {
            isolated: true,
            ..TaskArgs::default()
        };
        apply_task(&config, &mut isolated, Scope::NewBranch).unwrap();
        assert_eq!(grants(&isolated), ["github:read"]);
        // Not isolated: nowhere to place them.
        let mut shared = TaskArgs::default();
        apply_task(&config, &mut shared, Scope::NewBranch).unwrap();
        assert!(grants(&shared).is_empty());
        // The flags win, whole.
        let mut own = TaskArgs {
            isolated: true,
            provision: Some(branchyard::Provisioning {
                connectors: vec![branchyard::connectors::GrantEntry::parse("linear").unwrap()],
                ..Default::default()
            }),
            ..TaskArgs::default()
        };
        apply_task(&config, &mut own, Scope::NewBranch).unwrap();
        assert_eq!(grants(&own), ["linear:read"]);
        // A send keeps the branch's.
        let mut send = TaskArgs {
            isolated: true,
            ..TaskArgs::default()
        };
        apply_task(&config, &mut send, Scope::Continue).unwrap();
        assert!(grants(&send).is_empty());
    }

    #[test]
    fn continuing_takes_only_permissions() {
        let config = config(FILE);
        let mut task = TaskArgs::default();
        apply_task(&config, &mut task, Scope::Continue).unwrap();
        assert_eq!(task.permissions, Permissions::Ask);
        assert_eq!(task.harness, None);
        assert_eq!(task.provision, None);
        let mut yes = TaskArgs {
            permissions: Permissions::Yes,
            ..TaskArgs::default()
        };
        apply_task(&config, &mut yes, Scope::Continue).unwrap();
        assert_eq!(yes.permissions, Permissions::Yes, "the flag wins");
    }

    #[test]
    fn secrets_need_a_private_home() {
        let config =
            config("[secrets]\nOPENAI_API_KEY = \"OPENAI_API_KEY\"\n[defaults]\nmodel = \"m\"");
        let mut task = TaskArgs::default();
        apply_task(&config, &mut task, Scope::NewBranch).unwrap();
        assert!(task.provision.unwrap().secrets.is_empty());
    }

    #[test]
    fn remote_settings_follow_their_server_only() {
        let config = config(FILE);
        let mut globals = Globals::default();
        apply_globals(&config, &mut globals);
        assert_eq!(globals.remote.as_deref(), Some("http://127.0.0.1:9"));
        assert_eq!(globals.token_file.as_deref(), Some("/t/token"));
        let mut other = Globals {
            remote: Some("https://elsewhere".into()),
            ..Globals::default()
        };
        apply_globals(&config, &mut other);
        assert_eq!(other.token_file, None, "another server's token is not used");
    }

    #[test]
    fn serve_gets_the_configured_file_unless_given_one() {
        let config = config(FILE);
        assert_eq!(
            serve_args(&config, vec![]),
            ["--config", "/srv/server.json"]
        );
        assert_eq!(
            serve_args(&config, vec!["--config".into(), "x".into()]),
            ["--config", "x"]
        );
        assert_eq!(
            serve_args(&config, vec!["token".into(), "new".into()]),
            ["token", "new"]
        );
        // The token commands that read the server's store take the file
        // after them, unless they name their own place.
        let words = |w: &[&str]| -> Vec<String> { w.iter().map(|a| a.to_string()).collect() };
        assert_eq!(
            serve_args(&config, words(&["token", "new", "--link"])),
            ["token", "new", "--link", "--config", "/srv/server.json"]
        );
        assert_eq!(
            serve_args(&config, words(&["token", "revoke", "phone"])),
            ["token", "revoke", "phone", "--config", "/srv/server.json"]
        );
        assert_eq!(
            serve_args(&config, words(&["token", "list", "--data-dir", "d"])),
            ["token", "list", "--data-dir", "d"]
        );
        assert_eq!(
            serve_args(&config, vec!["--worker".into()]),
            ["--config", "/srv/server.json", "--worker"]
        );
        // The server's own spellings of --config, and help, keep theirs.
        for given in [
            &["-c", "x"][..],
            &["-cx"],
            &["--config=x"],
            &["-h"],
            &["--version"],
        ] {
            let given: Vec<String> = given.iter().map(|a| a.to_string()).collect();
            assert_eq!(serve_args(&config, given.clone()), given);
        }
    }
}
