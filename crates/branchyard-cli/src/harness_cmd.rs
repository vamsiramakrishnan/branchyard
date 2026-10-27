//! `by harnesses`: which harnesses a machine has (version, login, quota),
//! and installing, updating and logging in to them, here, over `ssh`, on a
//! recipe's machine, or on a server's workers. The detection, plans and
//! policy are the SDK's (`branchyard::inventory`); this picks the machine,
//! asks, renders and records. See docs/harness-lifecycle.md.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use branchyard::inventory::{
    self, DetectOptions, HarnessEvent, HarnessLog, HarnessState, InstallAction, InstallMode,
    InstallPolicy, Inventory, InventoryCache, LoginState, Permission, Quota, Standing,
};
use branchyard::HarnessInfo;
use branchyard_controls::catalog;
use branchyard_setup::config::HarnessInstall;
use serde_json::json;

use crate::args::{HarnessChange, HarnessesAction};
use crate::commands::{print, Env, Failure, Outcome, Target};
use crate::errors::{self, Report};
use crate::json;
use crate::render::{table, Cell, Column, Style, Tone};
use crate::ssh_remote::SshUrl;
use branchyard_recipe::quote;

/// The machine a command acts on.
enum Machine {
    Local,
    Ssh(SshUrl),
    Recipe(String),
}

impl Machine {
    fn parse(on: Option<&str>) -> Result<Machine, Failure> {
        let Some(on) = on else {
            return Ok(Machine::Local);
        };
        if let Some(name) = on.strip_prefix("recipe:") {
            if name.is_empty() {
                return Err(Report::usage("--on recipe:NAME needs a recipe's name")
                    .next("by recipe list (lists the recipes)")
                    .into());
            }
            return Ok(Machine::Recipe(name.to_owned()));
        }
        if let Some(rest) = on.strip_prefix("ssh://") {
            // A path, as in a `--remote` URL, is allowed and unused.
            let url = match rest.contains('/') {
                true => on.to_owned(),
                false => format!("{on}/~"),
            };
            return SshUrl::parse(&url).map(Machine::Ssh).map_err(|why| {
                Report::usage(format!("--on {on}: {why}"))
                    .next("pass --on ssh://[user@]host[:port]")
                    .into()
            });
        }
        Err(Report::usage(format!(
            "--on {on:?} is not ssh://[user@]host[:port] or recipe:NAME"
        ))
        .next("pass --on ssh://[user@]host[:port], or --on recipe:NAME (by recipe list lists them)")
        .into())
    }

    /// For a sentence: where, in a person's words. `name` is for the log
    /// and JSON, where a program reads it.
    fn place(&self) -> String {
        match self {
            Machine::Local => "this machine".into(),
            other => other.name(),
        }
    }

    /// For messages and the log.
    fn name(&self) -> String {
        match self {
            Machine::Local => "local".into(),
            Machine::Ssh(url) => {
                let user = url
                    .user
                    .as_deref()
                    .map(|u| format!("{u}@"))
                    .unwrap_or_default();
                let port = url.port.map(|p| format!(":{p}")).unwrap_or_default();
                format!("ssh://{user}{}{port}", url.host)
            }
            Machine::Recipe(name) => format!("recipe:{name}"),
        }
    }
}

/// `ssh` to `url`'s host: in batch mode, or with a terminal for a login.
fn ssh(url: &SshUrl, tty: bool) -> Command {
    let program = std::env::var("BRANCHYARD_SSH")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "ssh".into());
    let mut command = Command::new(program);
    match tty {
        true => command.arg("-t"),
        false => command.args(["-T", "-o", "BatchMode=yes"]),
    };
    if let Some(port) = url.port {
        command.arg("-p").arg(port.to_string());
    }
    if let Some(user) = &url.user {
        command.arg("-l").arg(user);
    }
    command.arg("--").arg(&url.host);
    command
}

/// `url`'s host as an `ssh` command line names it: `[-p PORT ][USER@]HOST`.
fn ssh_target(url: &SshUrl) -> String {
    format!(
        "{}{}{}",
        url.port.map(|p| format!("-p {p} ")).unwrap_or_default(),
        url.user
            .as_deref()
            .map(|u| format!("{u}@"))
            .unwrap_or_default(),
        url.host
    )
}

/// Where this user's harness state lives: beside the user configuration.
fn user_dir() -> PathBuf {
    crate::setup_io::user_file()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

fn cache() -> InventoryCache {
    InventoryCache::new(inventory::user_paths(&user_dir()).0, inventory::DEFAULT_TTL)
}

/// Forget this machine's cached inventory, so the next command that reads
/// it detects again: `by doctor --refresh` and `by setup --refresh`.
pub(crate) fn forget_inventory() {
    cache().clear();
}

/// The harnesses this machine has, from the cache `by harnesses` keeps
/// (detected afresh once it is older than its time to live), without the
/// usage meters. For `by doctor`.
pub(crate) fn detected_here() -> Result<Inventory, String> {
    let mut found = cache().local(&DetectOptions::from_env(), false)?;
    count_stored_keys(&mut found);
    Ok(found)
}

/// The keys `by harnesses login ID --api-key` stored count as logins, by
/// name: they are in `[secrets]`, which the inventory's script cannot see;
/// so do the provider logins `--provider` recorded in `[logins]`.
fn count_stored_keys(found: &mut Inventory) {
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    let vars = |name: &str| std::env::var(name).ok();
    let Some(config) = crate::defaults::config_at(&cwd, &vars).ok().flatten() else {
        return;
    };
    let root = crate::setup_io::project_root(&cwd);
    let stored = inventory::stored_keys(&root, &config.secrets);
    inventory::count_stored_keys(found, &stored);
    branchyard::configure::count_provider_logins(found, &config.logins, &stored);
}

/// The qualification of `id`'s default profile (else any profile of it).
pub(crate) fn qualification_of(known: &[HarnessInfo], id: &str) -> Option<String> {
    let mine = || known.iter().filter(|k| k.harness == id || k.profile == id);
    mine()
        .find(|k| k.default)
        .and_then(|k| k.qualification.clone())
        .or_else(|| mine().find_map(|k| k.qualification.clone()))
}

/// What this machine has of `ids` (catalog IDs), for the check before a
/// run: the cache `by harnesses` keeps when it is fresh and looked for
/// them, else a detection of just those, which is not kept.
pub(crate) fn detected_only(ids: &[String]) -> Result<Inventory, String> {
    let options = DetectOptions::from_env();
    let mut found = match cache().get(&options) {
        Some(found) if ids.iter().all(|id| found.checked(id)) => found,
        _ => inventory::detect_local(&DetectOptions {
            only: Some(ids.to_vec()),
            ..options
        })?,
    };
    // The keys `by harnesses login ID --api-key` stored are logins here as
    // in the listing: an isolated run gets them.
    count_stored_keys(&mut found);
    Ok(found)
}

fn log() -> HarnessLog {
    HarnessLog::new(inventory::user_paths(&user_dir()).1)
}

fn record(event: HarnessEvent) {
    if let Err(e) = log().append(&event) {
        eprintln!(
            "by: warning: could not record the {} in the harness log: {e}",
            event.action
        );
    }
}

/// The effective `[harnesses]` as a policy. A repository's file cannot
/// allow installs; reading it refuses one that tries (`check_layer`).
pub(crate) fn policy(env: &Env) -> Result<InstallPolicy, Failure> {
    let cwd = std::env::current_dir()?;
    let vars = |name: &str| std::env::var(name).ok();
    let config = crate::defaults::config_at(&cwd, &vars)
        .map_err(|why| errors::config(&why))?
        .map(|c| c.harnesses)
        .unwrap_or_default();
    let mode = config.install.map(|m| match m {
        HarnessInstall::Never => InstallMode::Never,
        HarnessInstall::Ask => InstallMode::Ask,
        HarnessInstall::Auto => InstallMode::Auto,
    });
    Ok(InstallPolicy::new(mode, config.allow, interactive(env)))
}

/// A refused install (`denied`, exit 4): the policy's reason, then the file
/// to edit and the key to set (the SDK's refusal says only "your user
/// configuration"), the command that runs it once allowed, or the command
/// to run by hand.
fn refused_install(
    policy: &InstallPolicy,
    id: &str,
    action: InstallAction,
    command: &str,
    why: &str,
) -> Report {
    let file = crate::setup_io::user_file();
    let file = file.display();
    let report = Report::new("denied", format!("not running `{command}`: {why}"));
    if !policy.allow.is_empty() && !policy.allow.iter().any(|a| a == id) {
        return report
            .line(
                "",
                format!("to allow it, add \"{id}\" to `allow` under [harnesses] in {file}"),
            )
            .next(format!("run `{command}` yourself"));
    }
    match policy.mode {
        InstallMode::Never => report
            .line(
                "",
                format!(
                    "to allow installs, put this in {file}:\n    [harnesses]\n    install = \"ask\""
                ),
            )
            .next(format!(
                "by harnesses {action} {id} --yes (once installs are allowed)"
            ))
            .next(format!("run `{command}` yourself")),
        _ => report.next(format!("by harnesses {action} {id} --yes")),
    }
}

/// Why there is no command to install `id` with, by what is missing: an
/// install command in the catalog (`unsupported`), a version the command
/// can take (`usage`), or the programs it needs on that machine
/// (`tool_unavailable`).
fn no_plan(
    id: &str,
    action: InstallAction,
    version: Option<&str>,
    tools: &std::collections::BTreeMap<String, String>,
    why: String,
) -> Report {
    if catalog::harness(id).is_some_and(|e| e.install.is_empty()) {
        return Report::new("unsupported", why).next(format!(
            "install {id} as its own documentation says, then by harnesses --refresh"
        ));
    }
    if version.is_some() && inventory::plan(id, action, None, tools).is_ok() {
        return Report::usage(why).next(format!("drop --version: by harnesses {action} {id}"));
    }
    Report::new("tool_unavailable", why)
        .next("install what one of its commands needs there, then run this again")
        .next("by harnesses --all (shows each harness's install commands)")
}

fn interactive(env: &Env) -> bool {
    env.stdin_tty && env.stderr_tty
}

/// Installing or logging in is a person's decision, never a harness's.
fn not_in_harness(what: &str) -> Result<(), Failure> {
    match crate::workspace_cmd::in_harness() {
        Some(branch) => Err(Report::new(
            "denied",
            format!(
                "a harness running on a branch ({branch}) cannot {what} harnesses; that is a \
                 person's decision, made outside any branch"
            ),
        )
        .next("run it from your own terminal, outside any branch")
        .into()),
        None => Ok(()),
    }
}

/// The catalog entry for `id`.
fn entry(id: &str) -> Result<&'static catalog::HarnessEntry, Failure> {
    catalog::harness(id).ok_or_else(|| {
        Report::new(
            "unknown_harness",
            format!("{id} is not a harness Branchyard knows"),
        )
        .next("by harnesses --all (lists them)")
        .into()
    })
}

// Detection.

/// Detect on `machine`; this machine's from the cache unless `refresh`.
fn detect(
    env: &Env,
    machine: &Machine,
    options: &DetectOptions,
    refresh: bool,
) -> Result<Inventory, Failure> {
    detect_aged(env, machine, options, refresh).map(|fetched| fetched.inventory)
}

/// [`detect`], with how old the cache's answer is when it gave one.
fn detect_aged(
    env: &Env,
    machine: &Machine,
    options: &DetectOptions,
    refresh: bool,
) -> Result<inventory::Fetched, Failure> {
    let fresh = |inventory| inventory::Fetched {
        inventory,
        cached: None,
    };
    match machine {
        Machine::Local => {
            let mut fetched = match options.only {
                Some(_) => inventory::detect_local(options).map(fresh),
                None => cache().fetch(options, refresh),
            }
            .map_err(|why| {
                Report::new(
                    "command_failed",
                    format!("could not detect the harnesses on this machine: {why}"),
                )
                .next("by harnesses --refresh (detects them again)")
            })?;
            attach_quota(&mut fetched.inventory);
            count_stored_keys(&mut fetched.inventory);
            Ok(fetched)
        }
        Machine::Ssh(url) => inventory::detect_with(options, true, |_| {
            let mut command = ssh(url, false);
            command.arg("sh -s");
            command
        })
        .map(fresh)
        .map_err(|why| {
            Report::new(
                "remote_unreachable",
                format!(
                    "could not detect the harnesses on {}: {why}",
                    machine.place()
                ),
            )
            .next(format!(
                "ssh -o BatchMode=yes {} true (reaches it without a prompt, as by does)",
                ssh_target(url)
            ))
            .into()
        }),
        Machine::Recipe(name) => on_recipe(env, name, |transport| {
            inventory::detect_with(options, false, |script| transport.command(script, &[]))
                .map(fresh)
                .map_err(|why| {
                    Report::new(
                        "remote_unreachable",
                        format!("could not detect the harnesses on recipe {name}'s machine: {why}"),
                    )
                    .next(format!(
                        "by recipe check {name} (makes a machine and runs a command in it)"
                    ))
                    .into()
                })
        }),
    }
}

/// Run `body` on a fresh machine from recipe `name`, destroyed afterwards.
fn on_recipe<T>(
    env: &Env,
    name: &str,
    body: impl FnOnce(&branchyard_recipe::Transport) -> Result<T, Failure>,
) -> Result<T, Failure> {
    use branchyard_sandbox::{SandboxProvider, SandboxSpec};
    let recipe = crate::recipe_cmd::trusted(env, name)?;
    let ssh = std::env::var("BRANCHYARD_SSH")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "ssh".into());
    let provider = branchyard_recipe::RecipeProvider::new(recipe, ssh);
    let machine = format!("by-harnesses-{name}-{}", std::process::id());
    eprintln!("by: creating a machine from recipe {name} to look at");
    provider.ensure(&SandboxSpec::new(&machine)).map_err(|e| {
        Report::new(
            "command_failed",
            format!("recipe {name} could not make a machine: {e}"),
        )
        .next(format!(
            "by recipe check {name} (runs its doctor, then makes a machine)"
        ))
    })?;
    let result = match provider.transport(&machine) {
        Some(transport) => body(&transport),
        None => Err(Report::new(
            "state",
            format!("recipe {name}: the machine has no way in (no connection was recorded for it)"),
        )
        .next(format!(
            "by recipe check {name} (makes a machine and runs a command in it)"
        ))
        .into()),
    };
    if let Err(e) = provider.destroy(&machine) {
        eprintln!("by: warning: could not destroy {machine}: {e}");
    }
    result
}

/// The usage meters `by usage` reads, on this machine's default logins.
fn attach_quota(found: &mut Inventory) {
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    let vars = |name: &str| std::env::var(name).ok();
    let config = crate::defaults::config_at(&cwd, &vars)
        .ok()
        .flatten()
        .map(|c| c.usage)
        .unwrap_or_default();
    let wanted = |id: &str| found.get(id).is_some();
    if !wanted("claude-code") && !wanted("codex") {
        return;
    }
    let logins = crate::usage::meter(&config, &vars, branchyard_support::time::now_ms());
    for login in logins.iter().filter(|l| l.account == "default" && l.found) {
        let Some(state) = found.harnesses.iter_mut().find(|h| h.id == login.harness) else {
            continue;
        };
        let windows = [&login.five_hour, &login.weekly];
        // The fuller window; the weekly one when they are level.
        let used = |w: &crate::usage::Window| w.used_percent.unwrap_or(0.0);
        let [five, weekly] = windows;
        let fuller = if used(five) > used(weekly) {
            five
        } else {
            weekly
        };
        state.quota = Some(Quota {
            five_hour_percent: login.five_hour.used_percent,
            weekly_percent: login.weekly.used_percent,
            limit_reached: windows.iter().any(|w| w.limit_reached),
            resets_at_ms: fuller.resets_at_ms,
        });
    }
}

// Commands.

/// `by harnesses [ACTION]`.
#[allow(clippy::too_many_arguments)] // ratchet: branchyard-cli
pub fn main(
    env: &Env,
    target: &Target,
    json: bool,
    all: bool,
    profiles: bool,
    refresh: bool,
    on: Option<&str>,
    action: Option<&HarnessesAction>,
) -> Outcome {
    if on.is_some() && matches!(target, Target::Remote(_)) {
        return Err(Report::usage(
            "--on names one machine and --remote a server's workers; give one",
        )
        .next("drop --on (a server's workers), or drop --remote (one machine)")
        .into());
    }
    match action {
        None if all => crate::catalog_cmd::harnesses(env, target, json),
        None if profiles => crate::commands::harnesses(env, target, json),
        None => list(env, target, json, refresh, on),
        Some(HarnessesAction::Install(change)) => {
            change_harness(env, target, json, on, change, InstallAction::Install)
        }
        Some(HarnessesAction::Update(change)) => {
            change_harness(env, target, json, on, change, InstallAction::Update)
        }
        Some(HarnessesAction::Login {
            id,
            provider: Some(provider),
            api_key,
            yes,
        }) => login_provider(
            env,
            target,
            json,
            on,
            &ProviderAsk {
                id,
                provider,
                new_key: *api_key,
                yes: *yes,
            },
        ),
        Some(HarnessesAction::Login {
            id,
            api_key,
            provider: None,
            yes,
        }) => login(env, target, json, on, id, *api_key, *yes),
        Some(HarnessesAction::Log) => show_log(env, json),
    }
}

#[allow(clippy::expect_used, clippy::unwrap_in_result)] // ratchet: branchyard-cli
fn list(env: &Env, target: &Target, as_json: bool, refresh: bool, on: Option<&str>) -> Outcome {
    if let Target::Remote(remote) = target {
        let report = remote.client.inventory()?;
        if as_json {
            return print(&json::text(
                &serde_json::to_value(&report).expect("serializes"),
            ));
        }
        if report.workers.is_empty() {
            return print("no live worker serves a repository you can see\n");
        }
        let mut text = String::new();
        for worker in &report.workers {
            let this = match worker.this {
                true => " (this server)",
                false => "",
            };
            text.push_str(&format!(
                "worker {} on {}{this}, serving {}\n",
                worker.id,
                worker.host,
                worker.repos.join(", ")
            ));
            match &worker.inventory {
                Some(found) => text.push_str(&inventory_text(found, false, None, env.style())),
                None => {
                    text.push_str("  advertises no harness inventory (run with inventory on)\n")
                }
            }
            text.push('\n');
        }
        return print(&text);
    }
    let machine = Machine::parse(on)?;
    let fetched = detect_aged(env, &machine, &DetectOptions::from_env(), refresh)?;
    if as_json {
        return print(&json::text(
            &serde_json::to_value(&fetched.inventory).expect("serializes"),
        ));
    }
    print(&inventory_text(
        &fetched.inventory,
        matches!(machine, Machine::Local),
        fetched.cached,
        env.style(),
    ))
}

fn quota_text(quota: Option<&Quota>) -> String {
    let Some(q) = quota else {
        return "-".into();
    };
    let percent = |p: Option<f64>| p.map_or_else(|| "-".into(), |p| format!("{p:.0}%"));
    let mut text = format!(
        "5h {} · wk {}",
        percent(q.five_hour_percent),
        percent(q.weekly_percent)
    );
    if q.limit_reached {
        text.push_str(" · limit reached");
    }
    text
}

/// The login cell: `Login::label`, green when logged in, red when logged out.
fn login_text(state: &HarnessState) -> (String, Tone) {
    let tone = match state.login.state {
        // A key the harness will not use is not a login to be pleased by.
        LoginState::LoggedIn if state.login.key_unused() => Tone::Yellow,
        LoginState::LoggedIn => Tone::Green,
        LoginState::LoggedOut => Tone::Red,
        LoginState::Unknown => Tone::Dim,
    };
    (state.login.label(), tone)
}

/// An inventory as a table with what is not ready and why.
/// `local`: the inventory is of this machine, which is how it is named.
/// `cached`: how old the cache's answer is, when the cache gave it.
fn inventory_text(
    found: &Inventory,
    local: bool,
    cached: Option<std::time::Duration>,
    style: Style,
) -> String {
    let known = branchyard::harnesses();
    let any_quota = found.harnesses.iter().any(|h| h.quota.is_some());
    let mut columns = vec![
        Column {
            header: "HARNESS",
            max: 20,
            right: false,
        },
        Column {
            header: "VERSION",
            max: 16,
            right: false,
        },
        Column {
            header: "LOGIN",
            // `key set (GOOGLE_GENERATIVE_AI_API_KEY)`, OpenCode's, is 38.
            max: 40,
            right: false,
        },
        Column {
            header: "LIVE RUN",
            max: 12,
            right: false,
        },
    ];
    if any_quota {
        columns.push(Column {
            header: "QUOTA",
            max: 22,
            right: false,
        });
    }
    columns.push(Column {
        header: "PATH",
        max: 48,
        right: false,
    });
    let rows: Vec<Vec<Cell>> = found
        .harnesses
        .iter()
        .map(|h| {
            let (login, tone) = login_text(h);
            let version = h.version_text();
            let mut row = vec![
                Cell::plain(&h.id),
                match (&h.version, h.too_old()) {
                    // A release the driver cannot follow; the line under
                    // the table says which and what to install.
                    (Some(_), Some(_)) => Cell::toned(format!("{version} (too old)"), Tone::Yellow),
                    (Some(_), None) => Cell::plain(version),
                    (None, _) => Cell::toned(version, Tone::Yellow),
                },
                Cell::toned(login, tone),
                match qualification_of(&known, &h.id)
                    .as_deref()
                    .and_then(inventory::live_score)
                {
                    Some(score) => Cell::plain(format!("live {score}")),
                    None => Cell::toned("no live run", Tone::Dim),
                },
            ];
            if any_quota {
                row.push(Cell::plain(quota_text(h.quota.as_ref())));
            }
            row.push(match h.on_path {
                true => Cell::plain(&h.path),
                false => Cell::toned(format!("{} (not on PATH)", h.path), Tone::Yellow),
            });
            row
        })
        .collect();
    let mut text = match rows.is_empty() {
        true => "no known harness is installed\n".to_owned(),
        false => table(&columns, &rows, style),
    };
    for h in &found.harnesses {
        match (&h.version, &h.version_note) {
            (None, Some(note)) if note.starts_with(inventory::SLOW_START) => {
                text.push_str(&format!(
                    "  {}: version: {note}; `by harnesses --refresh` asks again, and \
                 BRANCHYARD_HARNESS_TIMEOUT (seconds) sets how long to wait\n",
                    h.id
                ))
            }
            (None, Some(note)) => text.push_str(&format!("  {}: no version: {note}\n", h.id)),
            (Some(_), Some(note)) => text.push_str(&format!("  {}: version: {note}\n", h.id)),
            (_, None) => {}
        }
        match h.standing() {
            Standing::Ready => text.push_str(&format!("  {}: {}\n", h.id, h.login.detail)),
            Standing::NotVerified(why) => text.push_str(&format!("  {}: {why}\n", h.id)),
            Standing::CannotRun(why) => text.push_str(&format!("  {}: cannot run: {why}\n", h.id)),
        }
        if h.login.key_unused() {
            text.push_str(
                "  antigravity: `by harnesses login antigravity` writes modelProvider gemini \
                 into its settings, after showing you the change\n",
            );
        }
        if let Some(drift) = inventory::live_drift(
            h.version.as_deref(),
            qualification_of(&known, &h.id).as_deref(),
        ) {
            text.push_str(&format!("  {}: {drift}\n", h.id));
        } // What `by catalog refresh` found on npm, never fetched here.
        if let Some(latest) = crate::live_catalog::latest(&h.id) {
            if h.version
                .as_deref()
                .is_none_or(|v| !v.contains(&latest.latest))
            {
                text.push_str(&format!(
                    "  {}: npm's latest {} is {} (by harnesses update {} --version {})\n",
                    h.id, latest.package, latest.latest, h.id, latest.latest
                ));
            }
        }
    }
    text.push_str(&format!(
        "\n{} of the {} harnesses with a known executable are installed on {}{}. `by harnesses \
         --all` lists the rest with their install commands; `by harnesses install ID` installs \
         one.\n",
        found.harnesses.len(),
        found.checked.len(),
        match (local, found.host.as_str()) {
            (true, _) | (_, "") => "this machine",
            (false, host) => host,
        },
        // The cache answered: say so, and how to ask the harnesses again.
        cached
            .map(|age| format!(" ({})", inventory::cached_note(age)))
            .unwrap_or_default()
    ));
    text
}

#[allow(clippy::expect_used, clippy::unwrap_in_result)] // ratchet: branchyard-cli
fn change_harness(
    env: &Env,
    target: &Target,
    as_json: bool,
    on: Option<&str>,
    change: &HarnessChange,
    action: InstallAction,
) -> Outcome {
    let id = change.id.as_str();
    entry(id)?;
    not_in_harness(&action.to_string())?;
    if let Target::Remote(_) = target {
        return Err(
            Report::usage("a server's workers never install harnesses themselves")
                .next(format!(
                    "{action} it on a worker's machine, without --remote: by harnesses {action} \
                     {id} --on ssh://HOST"
                ))
                .next("put it in the worker's image")
                .into(),
        );
    }
    let machine = Machine::parse(on)?;
    let options = DetectOptions {
        only: Some(vec![id.to_owned()]),
        ..DetectOptions::from_env()
    };
    if let Machine::Recipe(name) = &machine {
        let plan = inventory::plan(id, action, change.version.as_deref(), &Default::default())
            .ok()
            .map(|p| p.command)
            .or_else(|| catalog::harness(id).and_then(|e| e.install.first().cloned()));
        return Err(Report::usage(
            "a recipe's machines are made fresh each time, so an install there would not last",
        )
        .next(format!(
            "add it to recipe {name}'s create script{}",
            plan.map(|p| format!(": {p}")).unwrap_or_default()
        ))
        .into());
    }
    let before = detect(env, &machine, &options, true)?;
    let current = before.get(id).cloned();
    if action == InstallAction::Install && current.is_some() && change.version.is_none() {
        let version = current
            .as_ref()
            .and_then(|c| c.version.clone())
            .unwrap_or_else(|| "version unknown".into());
        return match as_json {
            true => print(&json::text(&json!({
                "harness": id, "on": machine.name(), "already_installed": true,
                "state": current,
            }))),
            false => print(&format!(
                "{id} is already installed on {} ({version}); `by harnesses update {id}` updates \
                 it\n",
                machine.place()
            )),
        };
    }
    if let (InstallAction::Update, None, Some(latest)) =
        (action, &change.version, crate::live_catalog::latest(id))
    {
        eprintln!(
            "by: npm's latest {} is {}, as `by catalog refresh` cached it; pass --version {} to \
             take it",
            latest.package, latest.latest, latest.latest
        );
    }
    let plan = inventory::plan(id, action, change.version.as_deref(), &before.tools)
        .map_err(|why| no_plan(id, action, change.version.as_deref(), &before.tools, why))?;
    let policy = policy(env)?;
    let event = |outcome: &str, detail: Option<String>, after: Option<String>| HarnessEvent {
        at_ms: branchyard_support::time::now_ms(),
        on: machine.name(),
        harness: id.to_owned(),
        action,
        by: "by harnesses".into(),
        command: Some(plan.command.clone()),
        outcome: outcome.into(),
        version_before: current.as_ref().and_then(|c| c.version.clone()),
        version_after: after,
        detail,
    };
    match policy.permits(id, change.yes, interactive(env)) {
        Permission::Run => {}
        Permission::Ask => {
            let answer = crate::console::terminal_prompt(&format!(
                "by: {action} {id} on {} by running `{}`? [y/N] ",
                machine.place(),
                plan.command
            ))?;
            if !matches!(answer.trim(), "y" | "Y" | "yes" | "Yes") {
                record(event("refused", Some("not confirmed".into()), None));
                return Err(
                    Report::new("denied", format!("not confirmed; {id} was not changed"))
                        .next(format!(
                            "by harnesses {action} {id} --yes (runs `{}` without asking)",
                            plan.command
                        ))
                        .into(),
                );
            }
        }
        Permission::Refused(why) => {
            record(event("refused", Some(why.clone()), None));
            return Err(refused_install(&policy, id, action, &plan.command, &why).into());
        }
    }
    eprintln!("by: on {}: {}", machine.place(), plan.command);
    if let Some(why) = &plan.unpinned {
        eprintln!("by: unpinned: {why}");
    }
    let mut run = |command: &str| -> inventory::RunResult {
        match &machine {
            Machine::Local => inventory::run_local(command),
            Machine::Ssh(url) => {
                let output = ssh(url, false)
                    .arg(format!("sh -c {}", quote(&format!("exec 2>&1; {command}"))))
                    .stdin(Stdio::null())
                    .output()
                    .map_err(|e| format!("could not run ssh: {e}"))?;
                Ok((
                    output.status.code(),
                    String::from_utf8_lossy(&output.stdout).into_owned(),
                ))
            }
            Machine::Recipe(_) => unreachable!("refused above"),
        }
    };
    let mut again = |_: &str| -> Result<Inventory, String> {
        detect(env, &machine, &options, true).map_err(|e| e.to_string())
    };
    let result = inventory::install(&plan, current.as_ref(), &mut run, &mut again);
    if machine_is_local(&machine) {
        cache().clear();
    }
    let after = result.after.as_ref().and_then(|a| a.version.clone());
    record(event(
        match result.verified {
            true => "verified",
            false => "failed",
        },
        result.problem.clone(),
        after.clone(),
    ));
    let mut value = serde_json::to_value(&result).expect("serializes");
    value["on"] = json!(machine.name());
    match (result.verified, &result.after) {
        (true, _) if as_json => print(&json::text(&value)),
        (true, Some(state)) => print(&format!(
            "{} {id} {} on {}{}\n",
            match action {
                InstallAction::Update => "updated",
                _ => "installed",
            },
            state.version.as_deref().unwrap_or("(version unknown)"),
            machine.place(),
            match (&current, action) {
                (Some(c), InstallAction::Update) =>
                    format!(" (was {})", c.version.as_deref().unwrap_or("unknown")),
                _ => String::new(),
            }
        )),
        // Not verified: the attempt as `--json` prints a verified one, in
        // the error's `result`.
        _ => {
            let mut report = Report::new(
                "command_failed",
                format!(
                    "{action} of {id} on {} did not verify: {}",
                    machine.place(),
                    result.problem.clone().unwrap_or_default()
                ),
            );
            let tail = result.output_tail.trim();
            if !tail.is_empty() {
                report = report.line("output", tail);
            }
            Err(report
                .next(format!("by harnesses --refresh (detects {id} again)"))
                .field("result", value)
                .into())
        }
    }
}

fn machine_is_local(machine: &Machine) -> bool {
    matches!(machine, Machine::Local)
}

/// A login that ran nothing: the person has a step to do, which `report`
/// says (`manual`, exit 3, so `by harnesses login X && by run ...` stops
/// here). Recorded as `manual`; `--json` carries the harness, the machine,
/// `status` `manual` and `run` (what to run) in the error.
fn manual_login(id: &str, machine: &Machine, command: Option<String>, report: Report) -> Outcome {
    let run = manual_run(&report);
    record(HarnessEvent {
        at_ms: branchyard_support::time::now_ms(),
        on: machine.name(),
        harness: id.to_owned(),
        action: InstallAction::Login,
        by: "by harnesses".into(),
        command,
        outcome: "manual".into(),
        version_before: None,
        version_after: None,
        detail: Some(run.clone()),
    });
    Err(report
        .field("harness", json!(id))
        .field("on", json!(machine.name()))
        .field("status", json!("manual"))
        .field("run", json!(run))
        .into())
}

/// A `manual` report in one sentence: what is left, then what to run.
fn manual_run(report: &Report) -> String {
    match report.next.is_empty() {
        true => report.message.clone(),
        false => format!("{}; {}", report.message, report.next.join(", or ")),
    }
}

fn login(
    env: &Env,
    target: &Target,
    as_json: bool,
    on: Option<&str>,
    id: &str,
    api_key: bool,
    yes: bool,
) -> Outcome {
    let e = entry(id)?;
    not_in_harness("log in to")?;
    let command = e.login.clone();
    if let Target::Remote(remote) = target {
        // A worker has no terminal: say what to run where.
        let report = remote.client.inventory()?;
        let mut lines = Vec::new();
        // The workers that still need a login (or an install) from a person.
        let mut waiting: Vec<&str> = Vec::new();
        for worker in &report.workers {
            let state = worker.inventory.as_ref().and_then(|i| i.get(id));
            if !matches!(state, Some(s) if s.login.state == LoginState::LoggedIn) {
                waiting.push(&worker.host);
            }
            let what = match (state, &command) {
                (None, _) if worker.inventory.is_some() => format!(
                    "{id} is not installed; install it there first (by harnesses install {id} \
                     --on ssh://{})",
                    worker.host
                ),
                (Some(s), _) if s.login.state == LoginState::LoggedIn => {
                    format!("already logged in ({})", s.login.detail)
                }
                (_, Some(command)) => {
                    format!("run `{command}` there, in a terminal, as the worker's user")
                }
                (_, None) => format!(
                    "set {} in the worker's environment or its [secrets]",
                    inventory::key_variables(id).join(" or ")
                ),
            };
            lines.push(json!({ "worker": worker.id, "host": worker.host, "do": what }));
        }
        if !as_json {
            let text: String = match lines.is_empty() {
                true => "no live worker serves a repository you can see\n".into(),
                false => lines
                    .iter()
                    .map(|l| {
                        format!(
                            "{} on {}: {}\n",
                            l["worker"].as_str().unwrap_or(""),
                            l["host"].as_str().unwrap_or(""),
                            l["do"].as_str().unwrap_or("")
                        )
                    })
                    .collect(),
            };
            print(&text)?;
        }
        if waiting.is_empty() {
            return match as_json {
                true => print(&json::text(&json!({
                    "harness": id, "status": "done", "workers": lines,
                }))),
                false => Ok(()),
            };
        }
        // Only a person on those workers can do the rest: `manual`, exit 3,
        // each worker's step in `workers` (and on stdout above).
        let hosts = waiting.join(", ");
        let failure = Report::new(
            "manual",
            format!(
                "{id} needs a login on {} of the {} workers, and only a person there can do it",
                waiting.len(),
                report.workers.len()
            ),
        )
        .next(match &command {
            Some(command) => {
                format!("run `{command}` on {hosts}, in a terminal, as the worker's user")
            }
            None => format!(
                "set {} in the environment or [secrets] of the worker on {hosts}",
                inventory::key_variables(id).join(" or ")
            ),
        });
        let run = manual_run(&failure);
        return Err(failure
            .field("harness", json!(id))
            .field("status", json!("manual"))
            .field("run", json!(run))
            .field("workers", json!(lines))
            .into());
    }
    let machine = Machine::parse(on)?;
    if api_key {
        return store_key(env, as_json, &machine, id);
    }
    // Antigravity with a key in the environment has nothing to sign in to:
    // it needs its settings to select the provider that reads the key.
    if id == "antigravity"
        && matches!(machine, Machine::Local)
        && std::env::var_os("GEMINI_API_KEY").is_some_and(|v| !v.is_empty())
        && e.binaries
            .iter()
            .any(|b| crate::setup_io::which(b).is_some())
    {
        return antigravity_provider(env, as_json, yes);
    }
    let Some(mut command) = command else {
        let vars = inventory::key_variables(id);
        let program = e.binaries.first().map_or(id, String::as_str);
        let mut report = Report::new(
            "manual",
            format!("the catalog records no login command for {id}"),
        );
        report = match vars.is_empty() {
            true => report.next(format!(
                "start `{program}` and sign in inside it, then `by harnesses --refresh`{}",
                e.homepage
                    .as_deref()
                    .map(|h| format!(" ({h})"))
                    .unwrap_or_default()
            )),
            false => report
                .next(format!(
                    "give it an API key with `by harnesses login {id} --api-key` ({})",
                    vars.join(" or ")
                ))
                .next(format!("sign in inside `{program}` itself")),
        };
        // Another provider's key, where the harness can take one.
        if let Some(provider) = branchyard::logins::providers_of(id).first() {
            report = report.next(format!(
                "log it in to another provider with that provider's key: `by harnesses login \
                 {id} --provider {provider}`"
            ));
        }
        return manual_login(id, &machine, None, report);
    };
    if !matches!(machine, Machine::Recipe(_)) {
        // The executable detection found, which may be off PATH there.
        let options = DetectOptions {
            only: Some(vec![id.to_owned()]),
            ..DetectOptions::from_env()
        };
        let found = detect(env, &machine, &options, true)?;
        let Some(state) = found.get(id) else {
            return Err(Report::new(
                "harness_unavailable",
                format!("{id} is not installed on {}", machine.place()),
            )
            .next(format!(
                "by harnesses install {id}{}",
                match &machine {
                    Machine::Local => String::new(),
                    other => format!(" --on {}", other.name()),
                }
            ))
            .into());
        };
        if !state.on_path {
            let rest = command.split_once(' ').map_or("", |(_, r)| r);
            command = format!("{} {rest}", quote(&state.path))
                .trim_end()
                .to_owned();
        }
    }
    let command = command;
    let manual = |report: Report| manual_login(id, &machine, Some(command.clone()), report);
    let mut process = match &machine {
        Machine::Recipe(name) => {
            let vars = inventory::key_variables(id);
            let mut report = Report::new(
                "manual",
                "a recipe's machines are made fresh each time, so a login there would not last",
            )
            .next(format!("log in from recipe {name}'s create script"));
            if !vars.is_empty() {
                report = report.next(format!(
                    "give it {} through the recipe's env",
                    vars.join(" or ")
                ));
            }
            return manual(report);
        }
        _ if !interactive(env) => {
            let place = match &machine {
                Machine::Ssh(url) => format!("ssh -t {} {command}", ssh_target(url)),
                _ => command.clone(),
            };
            return manual(
                Report::new(
                    "manual",
                    format!("{id}'s login is interactive and there is no terminal here"),
                )
                .next(format!("run `{place}` in a terminal")),
            );
        }
        Machine::Local => {
            let mut c = Command::new("/bin/sh");
            c.arg("-c").arg(&command);
            c
        }
        Machine::Ssh(url) => {
            let mut c = ssh(url, true);
            c.arg(&command);
            c
        }
    };
    eprintln!("by: on {}: {command}", machine.place());
    // The harness's own flow, with this terminal: a URL or device code it
    // prints reaches the person directly.
    let status = process
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| {
            let fix = match &machine {
                Machine::Ssh(_) => {
                    "install OpenSSH's client, or set BRANCHYARD_SSH to the ssh to run"
                }
                _ => "run it yourself in a terminal",
            };
            Report::new(
                "tool_unavailable",
                format!("could not run `{command}`: {}", errors::plain_io(&e)),
            )
            .next(fix)
        })?;
    if machine_is_local(&machine) {
        cache().clear();
    }
    let options = DetectOptions {
        only: Some(vec![id.to_owned()]),
        ..DetectOptions::from_env()
    };
    let after = detect(env, &machine, &options, true)?;
    let state = after.get(id).map(|s| s.login.clone());
    record(HarnessEvent {
        at_ms: branchyard_support::time::now_ms(),
        on: machine.name(),
        harness: id.to_owned(),
        action: InstallAction::Login,
        by: "by harnesses".into(),
        command: Some(command.clone()),
        outcome: match status.success() {
            true => "ran".into(),
            false => "failed".into(),
        },
        version_before: None,
        version_after: None,
        detail: state.as_ref().map(|l| format!("{}: {}", l.state, l.detail)),
    });
    if !status.success() {
        // The flow ran and failed (`command_failed`, exit 1); `--json` keeps
        // `status` `failed`, the flow's own exit status and the login after.
        return Err(Report::new(
            "command_failed",
            format!("`{command}` exited with {status}"),
        )
        .next(format!(
            "by harnesses login {id}{} (runs it again)",
            match &machine {
                Machine::Local => String::new(),
                other => format!(" --on {}", other.name()),
            }
        ))
        .field("harness", json!(id))
        .field("on", json!(machine.name()))
        .field("status", json!("failed"))
        .field("command_exit", json!(status.code()))
        .field("login", json!(state))
        .into());
    }
    if as_json {
        return print(&json::text(&json!({
            "harness": id,
            "on": machine.name(),
            "status": "done",
            "exit": status.code(),
            "login": state,
        })));
    }
    match state {
        Some(login) => print(&format!(
            "{id} on {}: {} ({}: {})\n",
            machine.place(),
            login.state,
            login.evidence,
            login.detail
        )),
        None => print(&format!(
            "{id} ran its login; it is not found on {} now\n",
            machine.place()
        )),
    }
}

/// Where Antigravity keeps its settings, under the home.
const ANTIGRAVITY_SETTINGS: &str = ".gemini/antigravity-cli/settings.json";

/// `by harnesses login antigravity` with `GEMINI_API_KEY` set: `agy` reads
/// the key only when its `settings.json` has `"modelProvider": "gemini"`
/// (docs/harness-lifecycle.md). Merge that one key in, keeping everything
/// else in the file (and its order and indentation), show the change, and
/// write it when confirmed on a terminal or with `--yes`. A different
/// provider already chosen is never replaced.
fn antigravity_provider(env: &Env, as_json: bool, yes: bool) -> Outcome {
    let Some(home) = crate::setup_io::home() else {
        return Err(Report::new(
            "io",
            "no home directory, so Antigravity's settings cannot be found",
        )
        .next("set HOME, then by harnesses login antigravity")
        .into());
    };
    let path = home.join(ANTIGRAVITY_SETTINGS);
    let shown = format!("~/{ANTIGRAVITY_SETTINGS}");
    let existing = read_settings(&path, &shown)?;
    let merged = branchyard_setup::install::merge::json(
        existing.as_deref(),
        r#"{"modelProvider": "gemini"}"#,
        "",
    )
    .map_err(|why| {
        Report::new("config", format!("{shown}: {why}")).next(format!(
            "set \"modelProvider\": \"gemini\" in {shown} yourself, if Gemini is the provider \
             you want"
        ))
    })?;
    let before = existing.clone().unwrap_or_default();
    // `status` as every login says it: done, or manual when only the change
    // was shown and the person has the step to take.
    let reported = |wrote: bool, diff: &str, said: &str| -> Outcome {
        match as_json {
            true => print(&json::text(&json!({
                "harness": "antigravity", "status": "done",
                "settings": shown, "changed": !diff.is_empty(), "wrote": wrote, "diff": diff,
            }))),
            false => print(said),
        }
    };
    if merged == before {
        return reported(
            false,
            "",
            &format!(
                "antigravity already reads GEMINI_API_KEY: {shown} has modelProvider gemini. \
                 Nothing to sign in to; `by harnesses --refresh` shows it.\n"
            ),
        );
    }
    let diff = branchyard_setup::plan::unified_diff(&shown, &before, &merged);
    let interactive = interactive(env);
    let mut asked = false;
    let confirmed = match (yes, interactive) {
        (true, _) => true,
        (false, true) => {
            asked = true;
            eprint!("{diff}");
            let answer = crate::console::terminal_prompt(&format!(
                "by: GEMINI_API_KEY is set, and agy reads it only with modelProvider gemini in \
                 {shown}. Write that? [y/N] "
            ))?;
            matches!(answer.trim(), "y" | "Y" | "yes" | "Yes")
        }
        (false, false) => {
            // The change on stdout (in the error's `diff` with `--json`),
            // and `manual`, exit 3, as for any login that only says what
            // to run.
            if !as_json {
                print(&diff)?;
            }
            let report = Report::new(
                "manual",
                format!(
                    "GEMINI_API_KEY is set, and agy reads it only with modelProvider gemini in \
                     {shown}; not written: there is no terminal to ask on"
                ),
            )
            .next("by harnesses login antigravity --yes (writes exactly the change shown)");
            let run = manual_run(&report);
            return Err(report
                .field("harness", json!("antigravity"))
                .field("status", json!("manual"))
                .field("run", json!(run))
                .field("settings", json!(shown))
                .field("changed", json!(true))
                .field("wrote", json!(false))
                .field("diff", json!(diff))
                .into());
        }
    };
    if !confirmed {
        return Err(
            Report::new("denied", format!("not confirmed; {shown} was not changed"))
                .next("by harnesses login antigravity --yes (writes exactly the change shown)")
                .into(),
        );
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mode = std::fs::metadata(&path)
        .map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o777)
        .unwrap_or(0o644);
    crate::setup_io::write_file(&path, &merged, mode)?;
    record(HarnessEvent {
        at_ms: branchyard_support::time::now_ms(),
        on: "local".into(),
        harness: "antigravity".into(),
        action: InstallAction::Login,
        by: "by harnesses".into(),
        command: None,
        outcome: "wrote_settings".into(),
        version_before: None,
        version_after: None,
        detail: Some(format!("modelProvider gemini in {shown}")),
    });
    cache().clear();
    reported(
        true,
        &diff,
        &format!(
            "{}wrote {shown}: antigravity reads GEMINI_API_KEY now. `by harnesses --refresh` \
             shows it.\n",
            match asked {
                true => "",
                false => diff.as_str(),
            }
        ),
    )
}

/// Keep an API key for `id`'s key variable in a 0600 file beside the user
/// configuration, and name it in the user file's `[secrets]`, so branches
/// with a private home get it: the configuration's existing secrets path.
/// Never in a worktree, never on a command line.
fn store_key(env: &Env, as_json: bool, machine: &Machine, id: &str) -> Outcome {
    if !matches!(machine, Machine::Local) {
        return Err(
            Report::usage("an API key is stored on the machine that uses it")
                .next(format!("run by harnesses login {id} --api-key there"))
                .into(),
        );
    }
    let vars = inventory::key_variables(id);
    let Some(var) = vars.first() else {
        return Err(Report::new(
            "unsupported",
            format!("{id} reads no API key variable that Branchyard knows of"),
        )
        .next(format!("by harnesses login {id} (its own login)"))
        .into());
    };
    let key = read_key(env, var, id, &format!("by harnesses login {id} --api-key"))?;
    let path = key_file(var)?;
    // The user file is read before anything is written, so one that cannot
    // be read leaves the key file unwritten too.
    crate::user_config::read(&crate::setup_io::user_file())?;
    write_key(&path, &key)?;
    let named = name_secret(var, &path)?;
    record(HarnessEvent {
        at_ms: branchyard_support::time::now_ms(),
        on: "local".into(),
        harness: id.to_owned(),
        action: InstallAction::Login,
        by: "by harnesses".into(),
        command: None,
        outcome: "stored_key".into(),
        version_before: None,
        version_after: None,
        detail: Some(format!("{var} in {}", path.display())),
    });
    cache().clear();
    match as_json {
        true => print(&json::text(&json!({
            "harness": id, "status": "stored", "variable": var, "file": path,
            "secrets_entry_added": named,
        }))),
        false => print(&format!(
            "stored {var} for {id} in {} (0600){}; branches with a private home (--isolated, a \
             sandbox) get it. For harnesses run directly, export {var} from that file yourself.\n",
            path.display(),
            match named {
                true => format!(
                    ", and named it in [secrets] of {}",
                    crate::setup_io::user_file().display()
                ),
                false => String::new(),
            }
        )),
    }
}

/// A key for `var`, from the terminal with echo off or from standard
/// input, never shown. `command` is what to pipe it into.
fn read_key(env: &Env, var: &str, id: &str, command: &str) -> Result<String, Failure> {
    let key = match env.stdin_tty {
        true => {
            eprint!("by: paste the {var} for {id} (it is not shown), then press Enter: ");
            read_hidden()?
        }
        false => {
            let mut text = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
            text
        }
    };
    let key = key.trim();
    if key.is_empty() || key.contains(['\n', '\r']) {
        return Err(Report::usage("no key on standard input")
            .next(format!("printf %s \"$KEY\" | {command}"))
            .into());
    }
    Ok(key.to_owned())
}

/// A file of a harness's own settings in your home, or `None` when there
/// is none yet.
fn read_settings(path: &Path, shown: &str) -> Result<Option<String>, Failure> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Report::new(
            "io",
            format!("could not read {shown}: {}", errors::plain_io(&e)),
        )
        .next(format!("make {shown} readable, then run this again"))
        .into()),
    }
}

/// A user configuration file that does not parse (`config`, exit 3).
fn unparsable(file: &Path, error: impl std::fmt::Display) -> Report {
    errors::config(&format!("{}: {error}", file.display()))
        .next("by config validate (says what is wrong and where)")
}

/// Where a stored key for `var` lives: `secrets/VAR` beside the user
/// configuration. Never in a repository, where it could be committed.
fn key_file(var: &str) -> Result<PathBuf, Failure> {
    let path = user_dir().join("secrets").join(var);
    let cwd = std::env::current_dir()?;
    let root = crate::setup_io::project_root(&cwd);
    if branchyard_config::is_repository(&root) && path.starts_with(&root) {
        return Err(Report::new(
            "config",
            format!(
                "{} is inside the repository at {}, where a key could be committed",
                path.display(),
                root.display()
            ),
        )
        .next("set BRANCHYARD_USER_CONFIG to a file outside the repository")
        .into());
    }
    Ok(path)
}

/// Write `key` to `path`, 0600, in a 0700 directory.
fn write_key(path: &Path, key: &str) -> Result<(), Failure> {
    if let Some(dir) = path.parent() {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    crate::setup_io::write_file(path, key, 0o600)?;
    Ok(())
}

/// What `by harnesses login ID --provider NAME` was asked.
struct ProviderAsk<'a> {
    id: &'a str,
    provider: &'a str,
    /// `--api-key`: read a new key even when one is kept already.
    new_key: bool,
    /// `--yes`: write the harness's own configuration without asking.
    yes: bool,
}

/// `by harnesses login ID --provider NAME`: log a harness in to another
/// model provider with that provider's key ([`branchyard::logins`]).
/// The key is kept as `--api-key` keeps one, once per provider
/// (`ZAI_API_KEY`), unless `[secrets]` already names it; `[logins]` in the
/// user file records the choice, so isolated and sandboxed branches get
/// the endpoint, the token variable and the model. A run in your own home
/// gets nothing from Branchyard: this prints the `export` line, and writes
/// the harness's own configuration where a variable cannot select the
/// provider (Codex's `config.toml`, Antigravity's settings, OpenCode's
/// model) only after showing the change and being told yes.
fn login_provider(
    env: &Env,
    target: &Target,
    as_json: bool,
    on: Option<&str>,
    ask: &ProviderAsk<'_>,
) -> Outcome {
    use branchyard::logins;
    let ProviderAsk { id, provider, .. } = *ask;
    entry(id)?;
    not_in_harness("log in to")?;
    let login = logins::check(id, provider).map_err(|why| {
        let theirs = logins::providers_of(id);
        let report = Report::usage(why);
        match theirs.is_empty() {
            true => report.next(format!("by harnesses login {id} (its own login)")),
            false => theirs.iter().take(2).fold(report, |report, p| {
                report.next(format!("by harnesses login {id} --provider {p}"))
            }),
        }
    })?;
    let local = matches!(target, Target::Local) && matches!(Machine::parse(on)?, Machine::Local);
    if !local {
        return Err(
            Report::usage("a provider login is kept on the machine that runs the harness")
                .next(format!(
                    "run by harnesses login {id} --provider {provider} there"
                ))
                .into(),
        );
    }
    let title = logins::provider(provider).map_or(provider, |p| p.title);
    let user_file = crate::setup_io::user_file();
    let text = crate::user_config::read(&user_file)?;
    let parsed = match text.trim().is_empty() {
        true => Default::default(),
        false => branchyard_setup::config::parse(&text).map_err(|e| unparsable(&user_file, e))?,
    };
    // The key: a new one when asked for, else what `[secrets]` already
    // names, else the file an earlier login stored, else read now.
    let var = login.key;
    let path = key_file(var)?;
    let configured = parsed.secrets.get(var).cloned();
    let ours = format!("@{}", path.display());
    let (key, source) = match (ask.new_key, &configured) {
        (false, Some(source)) if *source == ours && path.is_file() => ("reused", ours),
        (false, Some(source)) => ("named", source.clone()),
        (false, None) if path.is_file() => ("reused", format!("@{}", path.display())),
        _ => {
            let command = format!("by harnesses login {id} --provider {provider}");
            let key = read_key(env, var, id, &command)?;
            write_key(&path, &key)?;
            ("stored", format!("@{}", path.display()))
        }
    };
    let named = match key {
        "named" => false,
        _ => name_secret(var, &path)?,
    };
    let logins_entry = name_login(id, provider)?;
    record(HarnessEvent {
        at_ms: branchyard_support::time::now_ms(),
        on: "local".into(),
        harness: id.to_owned(),
        action: InstallAction::Login,
        by: "by harnesses".into(),
        command: None,
        outcome: "provider".into(),
        version_before: None,
        version_after: None,
        detail: Some(format!("{provider}: {var} from {source}")),
    });
    cache().clear();
    // In your own home: the export line, reading the key where it is.
    let key_expr = match source.strip_prefix('@') {
        // As the configuration reads it: `~/` is the home, and a relative
        // path is beside the user file.
        Some(file) => {
            let file = match (file.strip_prefix("~/"), crate::setup_io::home()) {
                (Some(rest), Some(home)) => home.join(rest),
                _ => user_dir().join(file),
            };
            format!("$(cat {})", quote(&file.display().to_string()))
        }
        None => format!("${source}"),
    };
    let export = logins::exports(login, &key_expr);
    let config = own_config(env, login, as_json, ask.yes)?;
    let isolated: Vec<&str> = login.env.iter().map(|(name, _)| *name).collect();
    if as_json {
        return print(&json::text(&json!({
            "harness": id,
            "provider": provider,
            "status": "stored",
            "variable": var,
            "key": key,
            "source": source,
            "secrets_entry_added": named,
            "logins_entry": logins_entry,
            "isolated": { "env": isolated, "model": login.model },
            "export": export,
            "config": config.as_ref().map(OwnConfig::json),
        })));
    }
    let user = user_file.display();
    let mut out = match key {
        "stored" => format!("stored {var} in {} (0600)", path.display()),
        "reused" => format!("{id} uses the {var} stored in {}", path.display()),
        _ => format!("{id} uses [secrets] {var} ({source})"),
    };
    out.push_str(&match (named, logins_entry) {
        (true, _) => {
            format!(", named it in [secrets] and set [logins] {id} = \"{provider}\" in {user}.\n")
        }
        (false, "unchanged") => format!("; [logins] {id} = \"{provider}\" in {user} already.\n"),
        (false, _) => format!("; set [logins] {id} = \"{provider}\" in {user}.\n"),
    });
    let gets: Vec<String> = login
        .env
        .iter()
        .map(|(name, value)| match value {
            logins::Value::Key => format!("{name} (the key)"),
            logins::Value::Text(text) => format!("{name}={text}"),
        })
        .collect();
    out.push_str(&format!(
        "{id} through {title}: a branch with a private home (--isolated, a sandbox) gets {}{}.\n",
        gets.join(", "),
        match login.model {
            Some(model) => format!(", and model {model} unless --model names another"),
            None => String::new(),
        }
    ));
    out.push_str(&format!(
        "In your own home (`by run` without --isolated, or {id} itself), export:\n  {export}\n"
    ));
    if let Some(config) = &config {
        out.push_str(&config.text);
    }
    print(&out)
}

/// The harness's own configuration a provider login offers for a home
/// that is yours, and what became of it.
struct OwnConfig {
    shown: String,
    diff: String,
    wrote: bool,
    /// Why it was not written, when it was not and needed to be.
    why: Option<String>,
    /// For people.
    text: String,
}

impl OwnConfig {
    fn json(&self) -> serde_json::Value {
        json!({
            "file": self.shown, "changed": !self.diff.is_empty(), "wrote": self.wrote,
            "diff": self.diff, "why": self.why,
        })
    }
}

/// Offer `login`'s configuration change for your own home: show the diff,
/// and write it with `--yes`, or when asked on a terminal and told yes.
/// `None` when the login needs none.
fn own_config(
    env: &Env,
    login: &branchyard::logins::ProviderLogin,
    as_json: bool,
    yes: bool,
) -> Result<Option<OwnConfig>, Failure> {
    use branchyard::logins::Config;
    let Some((relative, edit)) = branchyard::logins::config_edit(login) else {
        return Ok(None);
    };
    let Some(home) = crate::setup_io::home() else {
        return Ok(None);
    };
    let path = home.join(relative);
    let shown = format!("~/{relative}");
    let existing = read_settings(&path, &shown)?;
    let before = existing.clone().unwrap_or_default();
    let unchanged = |text: String, why: Option<String>| OwnConfig {
        text,
        shown: shown.clone(),
        diff: String::new(),
        wrote: false,
        why,
    };
    // A JSON key is merged, keeping the file's order and indentation, and
    // never replaces another value; Codex's provider is reconciled as the
    // provisioner writes it into a private home.
    let merged = match login.config {
        Some(Config::Json { key, value, .. }) => branchyard_setup::install::merge::json(
            existing.as_deref(),
            &json!({ key: value }).to_string(),
            "",
        ),
        _ => branchyard::logins::apply(&edit, existing.as_deref()),
    };
    let merged = match merged {
        Ok(merged) => merged,
        Err(why) => {
            let why = format!("{why}; set it there yourself");
            return Ok(Some(unchanged(
                format!("{shown} was not changed: {why}\n"),
                Some(why),
            )));
        }
    };
    if merged == before {
        return Ok(Some(unchanged(
            format!("{shown} selects {} already.\n", login.provider),
            None,
        )));
    }
    let diff = branchyard_setup::plan::unified_diff(&shown, &before, &merged);
    let what = format!(
        "{} reads {} in your own home only with this change to {shown}",
        login.harness, login.provider
    );
    let not_written = |text: String| OwnConfig {
        text,
        shown: shown.clone(),
        diff: diff.clone(),
        wrote: false,
        why: Some("not confirmed".into()),
    };
    let confirmed = match (yes, interactive(env) && !as_json) {
        (true, _) => true,
        (false, true) => {
            eprint!("{diff}");
            let answer = crate::console::terminal_prompt(&format!("by: {what}. Write it? [y/N] "))?;
            matches!(answer.trim(), "y" | "Y" | "yes" | "Yes")
        }
        (false, false) => {
            return Ok(Some(not_written(format!(
                "{diff}{what}. Not written: there is no terminal to ask on. Add --yes to write \
                 exactly the change above.\n"
            ))))
        }
    };
    if !confirmed {
        return Ok(Some(not_written(format!(
            "not confirmed; {shown} was not changed\n"
        ))));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mode = std::fs::metadata(&path)
        .map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o777)
        .unwrap_or(0o644);
    crate::setup_io::write_file(&path, &merged, mode)?;
    record(HarnessEvent {
        at_ms: branchyard_support::time::now_ms(),
        on: "local".into(),
        harness: login.harness.to_owned(),
        action: InstallAction::Login,
        by: "by harnesses".into(),
        command: None,
        outcome: "wrote_settings".into(),
        version_before: None,
        version_after: None,
        detail: Some(format!("{} in {shown}", login.provider)),
    });
    Ok(Some(OwnConfig {
        text: format!(
            "{}wrote {shown}.\n",
            match yes {
                true => diff.as_str(),
                false => "",
            }
        ),
        shown,
        diff,
        wrote: true,
        why: None,
    }))
}

/// Set `[logins] ID = "PROVIDER"` in the user file: `added`, `replaced`
/// (another provider was named) or `unchanged`.
fn name_login(id: &str, provider: &str) -> Result<&'static str, Failure> {
    let file = crate::setup_io::user_file();
    let text = crate::user_config::read(&file)?;
    let parsed = match text.trim().is_empty() {
        true => Default::default(),
        false => branchyard_setup::config::parse(&text).map_err(|e| unparsable(&file, e))?,
    };
    let line = format!("{id} = {}", branchyard_setup::config::toml_string(provider));
    // A file this cannot edit line by line: the line to set by hand.
    let by_hand = |what: String| -> Failure {
        Report::new("config", what)
            .next(format!(
                "set {line} under [logins] in {} yourself",
                file.display()
            ))
            .into()
    };
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let section = lines.iter().position(|l| l.trim() == "[logins]");
    let outcome = match (parsed.logins.get(id), section) {
        (Some(existing), _) if existing == provider => return Ok("unchanged"),
        (Some(_), Some(at)) => {
            // The line of [logins], before the next table, that sets it.
            let end = lines[at + 1..]
                .iter()
                .position(|l| l.trim_start().starts_with('['))
                .map_or(lines.len(), |n| at + 1 + n);
            let sets = |l: &str| {
                l.split_once('=')
                    .is_some_and(|(key, _)| key.trim().trim_matches('"') == id)
            };
            let Some(found) = (at + 1..end).find(|&n| sets(&lines[n])) else {
                return Err(by_hand(format!(
                    "[logins] in {} sets {id} in a way this cannot edit",
                    file.display()
                )));
            };
            lines[found] = line.clone();
            "replaced"
        }
        (Some(_), None) => {
            return Err(by_hand(format!(
                "{} sets logins.{id} outside a [logins] table",
                file.display()
            )))
        }
        (None, Some(at)) => {
            lines.insert(at + 1, line.clone());
            "added"
        }
        (None, None) => {
            if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push("[logins]".into());
            lines.push(line.clone());
            "added"
        }
    };
    let mut updated = lines.join("\n");
    updated.push('\n');
    branchyard_setup::config::parse(&updated)
        .map_err(|e| by_hand(format!("setting [logins] {id} in {}: {e}", file.display())))?;
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    crate::setup_io::write_file(&file, &updated, 0o600)?;
    Ok(outcome)
}

/// Read a line from the terminal with echo off.
fn read_hidden() -> Result<String, Failure> {
    let stty = |arg: &str| {
        Command::new("stty")
            .arg(arg)
            .stdin(Stdio::inherit())
            .status()
            .is_ok_and(|s| s.success())
    };
    let hidden = stty("-echo");
    let mut line = String::new();
    let read = std::io::stdin().read_line(&mut line);
    if hidden {
        stty("echo");
    }
    eprintln!();
    read?;
    Ok(line)
}

/// Add `VAR = "@path"` to the user file's `[secrets]` unless it names
/// `var` already; whether it was added.
fn name_secret(var: &str, path: &Path) -> Result<bool, Failure> {
    let file = crate::setup_io::user_file();
    let text = crate::user_config::read(&file)?;
    let parsed = match text.trim().is_empty() {
        true => Default::default(),
        false => branchyard_setup::config::parse(&text).map_err(|e| unparsable(&file, e))?,
    };
    let reference = format!("@{}", path.display());
    match parsed.secrets.get(var) {
        Some(existing) if *existing == reference => return Ok(false),
        Some(existing) => {
            return Err(Report::new(
                "config",
                format!(
                    "[secrets] {var} in {} already reads {existing}; the key is in {}",
                    file.display(),
                    path.display()
                ),
            )
            .next(format!(
                "set {var} = {} under [secrets] in {} yourself, if you want this one",
                branchyard_setup::config::toml_string(&reference),
                file.display()
            ))
            .into())
        }
        None => {}
    }
    let line = format!(
        "{var} = {}\n",
        branchyard_setup::config::toml_string(&reference)
    );
    let mut lines: Vec<&str> = text.lines().collect();
    let updated = match lines.iter().position(|l| l.trim() == "[secrets]") {
        Some(at) => {
            lines.insert(at + 1, line.trim_end());
            let mut out = lines.join("\n");
            out.push('\n');
            out
        }
        None => {
            let mut out = text.clone();
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("[secrets]\n");
            out.push_str(&line);
            out
        }
    };
    branchyard_setup::config::parse(&updated).map_err(|e| {
        Report::new(
            "config",
            format!("adding [secrets] {var} to {}: {e}", file.display()),
        )
        .next(format!(
            "set {} under [secrets] in {} yourself",
            line.trim_end(),
            file.display()
        ))
    })?;
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    crate::setup_io::write_file(&file, &updated, 0o600)?;
    Ok(true)
}

#[allow(clippy::expect_used, clippy::unwrap_in_result)] // ratchet: branchyard-cli
fn show_log(env: &Env, as_json: bool) -> Outcome {
    let events = log().read()?;
    if as_json {
        return print(&json::text(
            &serde_json::to_value(&events).expect("serializes"),
        ));
    }
    if events.is_empty() {
        return print(
            "no harness has been installed, updated or logged in to through Branchyard here\n",
        );
    }
    let columns = [
        Column {
            header: "WHEN",
            max: 20,
            right: false,
        },
        Column {
            header: "ON",
            max: 28,
            right: false,
        },
        Column {
            header: "HARNESS",
            max: 16,
            right: false,
        },
        Column {
            header: "ACTION",
            max: 8,
            right: false,
        },
        Column {
            header: "OUTCOME",
            max: 10,
            right: false,
        },
        Column {
            header: "VERSION",
            max: 20,
            right: false,
        },
        Column {
            header: "BY",
            max: 14,
            right: false,
        },
    ];
    let rows: Vec<Vec<Cell>> = events
        .iter()
        .map(|e| {
            let version = match (&e.version_before, &e.version_after) {
                (Some(b), Some(a)) if a != b => format!("{b} → {a}"),
                (_, Some(a)) => a.clone(),
                (Some(b), None) => b.clone(),
                (None, None) => "-".into(),
            };
            vec![
                Cell::plain(branchyard_support::time::utc_minute(e.at_ms)),
                Cell::plain(&e.on),
                Cell::plain(&e.harness),
                Cell::plain(e.action.to_string()),
                Cell::toned(
                    &e.outcome,
                    match e.outcome.as_str() {
                        "verified" | "ran" | "stored_key" => Tone::Green,
                        "failed" => Tone::Red,
                        "manual" => Tone::Yellow,
                        _ => Tone::Dim,
                    },
                ),
                Cell::plain(version),
                Cell::plain(&e.by),
            ]
        })
        .collect();
    print(&table(&columns, &rows, env.style()))
}

/// For `by serve` and `by worker`: detection on this machine with the
/// usage meters attached, as a worker advertises it.
pub fn worker_inventory() -> Option<Inventory> {
    let mut found = inventory::detect_local(&DetectOptions::from_env())
        .map_err(|e| eprintln!("by: warning: could not detect the harnesses here: {e}"))
        .ok()?;
    attach_quota(&mut found);
    Some(found)
}

/// The router's view of this machine, for a routed `by run` or `by fan`
/// whose candidates run harnesses by name; `None` when none does.
pub fn route_gate(
    fleet: &branchyard::Fleet,
    options: &branchyard::TaskOptions,
    preview: bool,
) -> Option<std::sync::Arc<dyn inventory::HarnessGate>> {
    if options.command.is_some()
        || !matches!(options.provider, None | Some(branchyard::Provider::Local))
    {
        return None;
    }
    let by_name = fleet
        .entries
        .values()
        .flat_map(|e| e.candidates.iter())
        .any(|c| c.command.is_none());
    if !by_name {
        return None;
    }
    let detect = DetectOptions::from_env();
    let found = match cache().local(&detect, false) {
        Ok(found) => found,
        Err(e) => {
            eprintln!("by: warning: could not detect the harnesses here, so the router does not consult them: {e}");
            return None;
        }
    };
    let cwd = std::env::current_dir().ok()?;
    let vars = |name: &str| std::env::var(name).ok();
    let config = crate::defaults::config_at(&cwd, &vars)
        .ok()
        .flatten()
        .map(|c| c.harnesses)
        .unwrap_or_default();
    let mode = config.install.map(|m| match m {
        HarnessInstall::Never => InstallMode::Never,
        HarnessInstall::Ask => InstallMode::Ask,
        HarnessInstall::Auto => InstallMode::Auto,
    });
    let policy = InstallPolicy::new(mode, config.allow, false);
    let in_harness = crate::workspace_cmd::in_harness().is_some();
    let policy = match in_harness {
        // A harness's own `by` never installs anything.
        true => InstallPolicy {
            mode: InstallMode::Never,
            ..policy
        },
        false => policy,
    };
    let gate = inventory::LocalGate::new(found, policy, detect)
        .with_log(log())
        .with_cache(cache())
        .with_messages(|text| eprintln!("by: {text}"));
    Some(match preview {
        true => gate.preview().into_arc(),
        false => gate.into_arc(),
    })
}
