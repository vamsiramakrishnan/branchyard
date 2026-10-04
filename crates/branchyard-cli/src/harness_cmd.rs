//! `by harnesses`: which harnesses a machine has (version, login, quota),
//! and installing, updating and logging in to them, here, over `ssh`, on a
//! recipe's machine, or on a server's workers. The detection, plans and
//! policy are the SDK's (`branchyard::inventory`); this picks the machine,
//! asks, renders and records. See docs/harness-lifecycle.md.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use branchyard::inventory::{
    self, DetectOptions, Evidence, HarnessEvent, HarnessLog, HarnessState, InstallAction,
    InstallMode, InstallPolicy, Inventory, InventoryCache, LoginState, Permission, Quota,
};
use branchyard_controls::catalog;
use branchyard_setup::config::HarnessInstall;
use serde_json::json;

use crate::args::{HarnessChange, HarnessesAction};
use crate::commands::{print, Env, Failure, Outcome, Target};
use crate::json;
use crate::render::{table, Cell, Column, Style, Tone};
use crate::ssh_remote::{quote, SshUrl};

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
                return Err(Failure::Message(
                    "--on recipe:NAME needs a recipe's name".into(),
                ));
            }
            return Ok(Machine::Recipe(name.to_owned()));
        }
        if let Some(rest) = on.strip_prefix("ssh://") {
            // A path, as in a `--remote` URL, is allowed and unused.
            let url = match rest.contains('/') {
                true => on.to_owned(),
                false => format!("{on}/~"),
            };
            return SshUrl::parse(&url)
                .map(Machine::Ssh)
                .map_err(Failure::Message);
        }
        Err(Failure::Message(format!(
            "--on {on:?} is not ssh://[user@]host[:port] or recipe:NAME"
        )))
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
fn policy(env: &Env) -> Result<InstallPolicy, Failure> {
    let cwd = std::env::current_dir()?;
    let vars = |name: &str| std::env::var(name).ok();
    let config = crate::defaults::config_at(&cwd, &vars)
        .map_err(Failure::Message)?
        .map(|c| c.harnesses)
        .unwrap_or_default();
    let mode = config.install.map(|m| match m {
        HarnessInstall::Never => InstallMode::Never,
        HarnessInstall::Ask => InstallMode::Ask,
        HarnessInstall::Auto => InstallMode::Auto,
    });
    Ok(InstallPolicy::new(mode, config.allow, interactive(env)))
}

fn interactive(env: &Env) -> bool {
    env.stdin_tty && env.stderr_tty
}

/// Installing or logging in is a person's decision, never a harness's.
fn not_in_harness(what: &str) -> Result<(), Failure> {
    match crate::workspace_cmd::in_harness() {
        Some(branch) => Err(Failure::Message(format!(
            "a harness running on a branch ({branch}) cannot {what} harnesses; that is a \
             person's decision, made outside any branch"
        ))),
        None => Ok(()),
    }
}

/// The catalog entry for `id`.
fn entry(id: &str) -> Result<&'static catalog::HarnessEntry, Failure> {
    catalog::harness(id).ok_or_else(|| {
        Failure::Message(format!(
            "{id} is not a harness Branchyard knows; `by harnesses --all` lists them"
        ))
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
    match machine {
        Machine::Local => {
            let mut found = match options.only {
                Some(_) => inventory::detect_local(options),
                None => cache().local(options, refresh),
            }
            .map_err(Failure::Message)?;
            attach_quota(&mut found);
            Ok(found)
        }
        Machine::Ssh(url) => inventory::detect_with(options, true, |_| {
            let mut command = ssh(url, false);
            command.arg("sh -s");
            command
        })
        .map_err(|e| Failure::Message(format!("on {}: {e}", machine.name()))),
        Machine::Recipe(name) => on_recipe(env, name, |transport| {
            inventory::detect_with(options, false, |script| transport.command(script, &[]))
                .map_err(Failure::Message)
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
    provider
        .ensure(&SandboxSpec::new(&machine))
        .map_err(|e| Failure::Message(format!("recipe {name}: {e}")))?;
    let result = match provider.transport(&machine) {
        Some(transport) => body(&transport),
        None => Err(Failure::Message(format!(
            "recipe {name}: the machine has no way in"
        ))),
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
        let fuller = windows
            .iter()
            .max_by(|a, b| {
                a.used_percent
                    .unwrap_or(0.0)
                    .total_cmp(&b.used_percent.unwrap_or(0.0))
            })
            .expect("two windows");
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
#[allow(clippy::too_many_arguments)]
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
        return Err(Failure::Message(
            "--on names one machine and --remote a server's workers; give one".into(),
        ));
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
        Some(HarnessesAction::Login { id, api_key }) => login(env, target, json, on, id, *api_key),
        Some(HarnessesAction::Log) => show_log(env, json),
    }
}

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
                Some(found) => text.push_str(&inventory_text(found, env.style())),
                None => {
                    text.push_str("  advertises no harness inventory (run with inventory on)\n")
                }
            }
            text.push('\n');
        }
        return print(&text);
    }
    let machine = Machine::parse(on)?;
    let found = detect(env, &machine, &DetectOptions::from_env(), refresh)?;
    if as_json {
        return print(&json::text(
            &serde_json::to_value(&found).expect("serializes"),
        ));
    }
    print(&inventory_text(&found, env.style()))
}

fn quota_text(quota: Option<&Quota>) -> String {
    let Some(q) = quota else {
        return "-".into();
    };
    let percent = |p: Option<f64>| p.map(|p| format!("{p:.0}%")).unwrap_or_else(|| "-".into());
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

fn login_text(state: &HarnessState) -> (String, Tone) {
    let l = &state.login;
    let how = match l.evidence {
        Evidence::Verified => "verified",
        Evidence::Likely => "likely",
        Evidence::None => "",
    };
    let tone = match l.state {
        LoginState::LoggedIn => Tone::Green,
        LoginState::LoggedOut => Tone::Red,
        LoginState::Unknown => Tone::Dim,
    };
    match how {
        "" => (l.state.to_string(), tone),
        how => (format!("{} ({how})", l.state), tone),
    }
}

/// An inventory as a table with what is not ready and why.
fn inventory_text(found: &Inventory, style: Style) -> String {
    let columns = [
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
            max: 24,
            right: false,
        },
        Column {
            header: "QUOTA",
            max: 22,
            right: false,
        },
        Column {
            header: "PATH",
            max: 48,
            right: false,
        },
    ];
    let rows: Vec<Vec<Cell>> = found
        .harnesses
        .iter()
        .map(|h| {
            let (login, tone) = login_text(h);
            vec![
                Cell::plain(&h.id),
                match &h.version {
                    Some(v) => Cell::plain(v),
                    None => Cell::toned("?", Tone::Yellow),
                },
                Cell::toned(login, tone),
                Cell::plain(quota_text(h.quota.as_ref())),
                match h.on_path {
                    true => Cell::plain(&h.path),
                    false => Cell::toned(format!("{} (not on PATH)", h.path), Tone::Yellow),
                },
            ]
        })
        .collect();
    let mut text = match rows.is_empty() {
        true => "no known harness is installed\n".to_owned(),
        false => table(&columns, &rows, style),
    };
    for h in &found.harnesses {
        if let Some(note) = &h.version_note {
            text.push_str(&format!("  {}: no version: {note}\n", h.id));
        }
        match h.ready() {
            Ok(()) => text.push_str(&format!("  {}: {}\n", h.id, h.login.detail)),
            Err(why) => text.push_str(&format!("  {}: cannot run: {why}\n", h.id)),
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
        "\n{} of the {} harnesses with a known executable are installed on {}. `by harnesses \
         --all` lists the rest with their install commands; `by harnesses install ID` installs \
         one.\n",
        found.harnesses.len(),
        found.checked.len(),
        match found.host.as_str() {
            "" => "this machine",
            host => host,
        }
    ));
    text
}

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
        return Err(Failure::Message(format!(
            "a server's workers never install harnesses themselves; {action} {id} on a worker's \
             machine with `by harnesses {action} {id} --on ssh://HOST`, or in its image"
        )));
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
        return Err(Failure::Message(format!(
            "a recipe's machines are made fresh each time, so an install there would not last; \
             add it to recipe {name}'s create script{}",
            plan.map(|p| format!(": {p}")).unwrap_or_default()
        )));
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
                machine.name()
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
        .map_err(Failure::Message)?;
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
                machine.name(),
                plan.command
            ))?;
            if !matches!(answer.trim(), "y" | "Y" | "yes" | "Yes") {
                record(event("refused", Some("not confirmed".into()), None));
                return Err(Failure::Message(format!(
                    "not confirmed; {id} was not changed"
                )));
            }
        }
        Permission::Refused(why) => {
            record(event("refused", Some(why.clone()), None));
            return Err(Failure::Message(format!(
                "not running `{}`: {why}",
                plan.command
            )));
        }
    }
    eprintln!("by: on {}: {}", machine.name(), plan.command);
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
    if as_json {
        let mut value = serde_json::to_value(&result).expect("serializes");
        value["on"] = json!(machine.name());
        print(&json::text(&value))?;
        return match result.verified {
            true => Ok(()),
            false => Err(Failure::Reported),
        };
    }
    match (result.verified, &result.after) {
        (true, Some(state)) => print(&format!(
            "{} {id} {} on {}{}\n",
            match action {
                InstallAction::Update => "updated",
                _ => "installed",
            },
            state.version.as_deref().unwrap_or("(version unknown)"),
            machine.name(),
            match (&current, action) {
                (Some(c), InstallAction::Update) =>
                    format!(" (was {})", c.version.as_deref().unwrap_or("unknown")),
                _ => String::new(),
            }
        )),
        _ => Err(Failure::Message(format!(
            "{action} of {id} on {} did not verify: {}{}",
            machine.name(),
            result.problem.unwrap_or_default(),
            match result.output_tail.trim() {
                "" => String::new(),
                tail => format!("\n{tail}"),
            }
        ))),
    }
}

fn machine_is_local(machine: &Machine) -> bool {
    matches!(machine, Machine::Local)
}

fn login(
    env: &Env,
    target: &Target,
    as_json: bool,
    on: Option<&str>,
    id: &str,
    api_key: bool,
) -> Outcome {
    let e = entry(id)?;
    not_in_harness("log in to")?;
    let command = e.login.clone();
    if let Target::Remote(remote) = target {
        // A worker has no terminal: say what to run where.
        let report = remote.client.inventory()?;
        let mut lines = Vec::new();
        for worker in &report.workers {
            let state = worker.inventory.as_ref().and_then(|i| i.get(id));
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
        if as_json {
            return print(&json::text(&json!({ "harness": id, "workers": lines })));
        }
        if lines.is_empty() {
            return print("no live worker serves a repository you can see\n");
        }
        let text: String = lines
            .iter()
            .map(|l| {
                format!(
                    "{} on {}: {}\n",
                    l["worker"].as_str().unwrap_or(""),
                    l["host"].as_str().unwrap_or(""),
                    l["do"].as_str().unwrap_or("")
                )
            })
            .collect();
        return print(&text);
    }
    let machine = Machine::parse(on)?;
    if api_key {
        return store_key(env, as_json, &machine, id);
    }
    let Some(mut command) = command else {
        let vars = inventory::key_variables(id);
        return Err(Failure::Message(format!(
            "the catalog records no login command for {id}{}",
            match vars.is_empty() {
                true => e
                    .homepage
                    .as_deref()
                    .map(|h| format!("; see {h}"))
                    .unwrap_or_default(),
                false => format!(
                    "; give it an API key with `by harnesses login {id} --api-key` ({})",
                    vars.join(" or ")
                ),
            }
        )));
    };
    if !matches!(machine, Machine::Recipe(_)) {
        // The executable detection found, which may be off PATH there.
        let options = DetectOptions {
            only: Some(vec![id.to_owned()]),
            ..DetectOptions::from_env()
        };
        let found = detect(env, &machine, &options, true)?;
        let Some(state) = found.get(id) else {
            return Err(Failure::Message(format!(
                "{id} is not installed on {}; `by harnesses install {id}{}` first",
                machine.name(),
                match &machine {
                    Machine::Local => String::new(),
                    other => format!(" --on {}", other.name()),
                }
            )));
        };
        if !state.on_path {
            let rest = command.split_once(' ').map(|(_, r)| r).unwrap_or("");
            command = format!("{} {rest}", quote(&state.path))
                .trim_end()
                .to_owned();
        }
    }
    let command = command;
    let reported = |how: String| -> Outcome {
        record(HarnessEvent {
            at_ms: branchyard_support::time::now_ms(),
            on: machine.name(),
            harness: id.to_owned(),
            action: InstallAction::Login,
            by: "by harnesses".into(),
            command: Some(command.clone()),
            outcome: "reported".into(),
            version_before: None,
            version_after: None,
            detail: Some(how.clone()),
        });
        match as_json {
            true => print(&json::text(
                &json!({ "harness": id, "on": machine.name(), "run": how }),
            )),
            false => print(&format!("{how}\n")),
        }
    };
    let mut process = match &machine {
        Machine::Recipe(name) => {
            return reported(format!(
                "a recipe's machines are made fresh each time; log in from recipe {name}'s create \
                 script, or give it {} through the recipe's env",
                inventory::key_variables(id).join(" or ")
            ))
        }
        _ if !interactive(env) => {
            let place = match &machine {
                Machine::Ssh(url) => format!(
                    "ssh -t {}{} {command}",
                    url.user
                        .as_deref()
                        .map(|u| format!("{u}@"))
                        .unwrap_or_default(),
                    url.host
                ),
                _ => command.clone(),
            };
            return reported(format!(
                "{id}'s login is interactive and there is no terminal here; run `{place}` in a \
                 terminal"
            ));
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
    eprintln!("by: on {}: {command}", machine.name());
    // The harness's own flow, with this terminal: a URL or device code it
    // prints reaches the person directly.
    let status = process
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| Failure::Message(format!("could not run `{command}`: {e}")))?;
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
    if as_json {
        return print(&json::text(&json!({
            "harness": id, "on": machine.name(), "exit": status.code(), "login": state,
        })));
    }
    match (status.success(), state) {
        (true, Some(login)) => print(&format!(
            "{id} on {}: {} ({}: {})\n",
            machine.name(),
            login.state,
            login.evidence,
            login.detail
        )),
        (true, None) => print(&format!(
            "{id} ran its login; it is not found on {} now\n",
            machine.name()
        )),
        (false, _) => Err(Failure::Message(format!(
            "`{command}` exited with {status}"
        ))),
    }
}

/// Keep an API key for `id`'s key variable in a 0600 file beside the user
/// configuration, and name it in the user file's `[secrets]`, so branches
/// with a private home get it: the configuration's existing secrets path.
/// Never in a worktree, never on a command line.
fn store_key(env: &Env, as_json: bool, machine: &Machine, id: &str) -> Outcome {
    if !matches!(machine, Machine::Local) {
        return Err(Failure::Message(format!(
            "an API key is stored on the machine that uses it; run `by harnesses login {id} \
             --api-key` there"
        )));
    }
    let vars = inventory::key_variables(id);
    let Some(var) = vars.first() else {
        return Err(Failure::Message(format!(
            "{id} reads no API key variable that Branchyard knows of; use `by harnesses login {id}`"
        )));
    };
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
        return Err(Failure::Message(format!(
            "no key on standard input: pipe it in, as in `printf %s \"$KEY\" | by harnesses login \
             {id} --api-key`"
        )));
    }
    let dir = user_dir().join("secrets");
    let path = dir.join(var);
    // Never in a repository: a key file there could be committed.
    let cwd = std::env::current_dir()?;
    let root = crate::setup_io::project_root(&cwd);
    if root.join(".git").exists() && path.starts_with(&root) {
        return Err(Failure::Message(format!(
            "{} is inside the repository at {}; set BRANCHYARD_USER_CONFIG outside it",
            path.display(),
            root.display()
        )));
    }
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
    }
    crate::setup_io::write_file(&path, key, 0o600)?;
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
            "harness": id, "variable": var, "file": path, "secrets_entry_added": named,
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
    let text = std::fs::read_to_string(&file).unwrap_or_default();
    let parsed = match text.trim().is_empty() {
        true => Default::default(),
        false => branchyard_setup::config::parse(&text)
            .map_err(|e| Failure::Message(format!("{}: {e}", file.display())))?,
    };
    let reference = format!("@{}", path.display());
    match parsed.secrets.get(var) {
        Some(existing) if *existing == reference => return Ok(false),
        Some(existing) => {
            return Err(Failure::Message(format!(
                "[secrets] {var} in {} already reads {existing}; the key is in {} — point it \
                 there yourself if you want this one",
                file.display(),
                path.display()
            )))
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
        Failure::Message(format!("adding [secrets] {var} to {}: {e}", file.display()))
    })?;
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    crate::setup_io::write_file(&file, &updated, 0o600)?;
    Ok(true)
}

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
