//! The checks, one function each, in the order the report lists them.
//!
//! A check reads the [`Context`] and returns its rows. It uses what the
//! rest of `by` already knows (the config loader, the trust decision, the
//! harness inventory, the engine's provider check) and says in one line what
//! it found and, when it is not `ok`, what to do. A check that cannot run
//! because an earlier one failed is a `warn` that names that check, so the
//! report never claims what it did not look at.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use branchyard::doctor::{self, HeldKind, HeldSandbox, SchemaState};
use branchyard::inventory::{self, HarnessState, LoginState, Standing};
use branchyard::models::Api;
use branchyard::services::{Service, ServiceState, ServiceStore};
use branchyard::{HarnessInfo, SandboxKeep, SandboxOptions, Yard};
use branchyard_client::api::ProblemList;
use branchyard_client::Client;
use branchyard_setup::config::{
    Microsandbox, NetworkEnforceMode, ProjectConfig, ProviderKind as ConfiguredProvider,
};
use branchyard_setup::probe::{Probe as _, KNOWN_VARIABLES};
use branchyard_support::time::now_ms;
use branchyard_workspace::Repository;

use super::context::{Context, Layer, Merged, Store};
use super::{Check, Status, NOT_CHECKED};
use crate::args::Globals;
use crate::readiness::Answer;
use crate::render::age_text;
use crate::workspace_cmd::{self, TrustState};

/// How long the whole question to a configured server may take.
const SERVER_DEADLINE: Duration = Duration::from_secs(5);

/// The harness `by run` starts when nothing names one.
const BUILT_IN_HARNESS: &str = "claude-code";

/// A check that could not run because `first` failed or did not run.
fn skipped(name: &'static str, because: &str, first: &str) -> Check {
    Check::warn(
        name,
        format!("{NOT_CHECKED}{because}"),
        format!("fix {first} first"),
    )
}

// Mode.

/// The mode this repository runs in, for the line above the report: where
/// harnesses run, where the state is, and whether a server is involved.
/// Read from the same effective configuration the checks use.
pub(super) fn mode(ctx: &Context) -> String {
    let Some(config) = ctx.config() else {
        return "not known: the configuration does not load (see config.merged)".to_owned();
    };
    if let Some(url) = ctx.globals.remote.as_deref() {
        return format!(
            "server: {} · state is kept by the server",
            without_userinfo(url)
        );
    }
    let run = match config
        .defaults
        .provider
        .unwrap_or(ConfiguredProvider::Local)
    {
        ConfiguredProvider::Local => match config.defaults.isolated {
            Some(true) => "local, isolated (private HOME, no sandbox)".to_owned(),
            _ => "local, no isolation".to_owned(),
        },
        ConfiguredProvider::Microsandbox => "sandbox: microsandbox".to_owned(),
        ConfiguredProvider::Container => "native sandbox: Container".to_owned(),
        ConfiguredProvider::AgentSandbox => "native sandbox: Kubernetes Agent Sandbox".to_owned(),
        ConfiguredProvider::Substrate => "remote sandbox: Agent Substrate".to_owned(),
        ConfiguredProvider::Recipe => format!(
            "sandbox: recipe {}",
            config.defaults.recipe.as_deref().unwrap_or("(none named)")
        ),
    };
    let mut mode = format!("{run} · SQLite (.branchyard/state.db) · no server");
    if serve_uses_postgres(ctx, config) {
        mode.push_str(" · by serve here uses PostgreSQL");
    }
    mode
}

/// `[serve] config` names a server file whose `database` is a PostgreSQL URL.
/// The URL is not shown: it may hold a password.
fn serve_uses_postgres(ctx: &Context, config: &ProjectConfig) -> bool {
    let Some(file) = config.serve.config.as_deref() else {
        return false;
    };
    let root = ctx.root().unwrap_or(&ctx.base);
    // An unreadable file is the `serve.config` row's finding; the Mode line
    // then says SQLite.
    branchyard_support::best_effort(
        "read the [serve] config file for the Mode line",
        std::fs::read_to_string(crate::setup_io::resolve(root, file)),
    )
    .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
    .and_then(|v| v["database"].as_str().map(str::to_owned))
    .is_some_and(|url| url.starts_with("postgres://") || url.starts_with("postgresql://"))
}

/// `url` without the `user:password@` of its authority.
pub(crate) fn without_userinfo(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let (authority, path) = rest
                .split_once('/')
                .map_or((rest, None), |(a, p)| (a, Some(p)));
            let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
            match path {
                Some(path) => format!("{scheme}://{host}/{path}"),
                None => format!("{scheme}://{host}"),
            }
        }
        None => url.to_owned(),
    }
}

// Repository.

/// Inside a git repository, with a commit to start branches from.
pub(super) fn repository_git(ctx: &Context) -> Vec<Check> {
    const NAME: &str = "repository.git";
    let root = match &ctx.repository {
        Ok(root) => root,
        Err(why) => {
            return vec![Check::fail(
                NAME,
                why,
                "run `git init` in the project, or cd into a repository (or pass --path)",
            )]
        }
    };
    let repository = match Repository::open(root) {
        Ok(repository) => repository,
        Err(error) => return vec![Check::fail(NAME, error.to_string(), "check that git works")],
    };
    let branch = match repository.current_branch() {
        Ok(Some(branch)) => format!("on branch {branch}"),
        Ok(None) => "on a detached HEAD".to_owned(),
        Err(error) => format!("branch unknown: {error}"),
    };
    let shown = format!("{} ({branch})", root.display());
    match repository.resolve("HEAD") {
        Ok(_) => vec![Check::ok(NAME, shown)],
        Err(_) => vec![Check::warn(
            NAME,
            format!("{shown}, with no commit yet"),
            "make a first commit: branches start from one",
        )],
    }
}

/// `.branchyard/` exists and its store opens.
pub(super) fn repository_state(ctx: &Context) -> Vec<Check> {
    const NAME: &str = "repository.state";
    vec![match ctx.store() {
        Store::Open(yard) => match yard.branches() {
            Ok(branches) => Check::ok(
                NAME,
                format!(
                    "{}/.branchyard opens; {} branch{}",
                    yard.root().display(),
                    branches.len(),
                    if branches.len() == 1 { "" } else { "es" }
                ),
            ),
            Err(error) => Check::fail(
                NAME,
                format!("the store opened but its branches cannot be read: {error}"),
                "the state is .branchyard/state.db (docs/durability.md)",
            ),
        },
        Store::NoRepository => skipped(NAME, "there is no repository", "repository.git"),
        // Where everyone starts: nothing is broken, a step is not taken.
        Store::Absent => Check::todo(
            NAME,
            format!(
                "not set up yet (no .branchyard/ in {})",
                ctx.root()
                    .map(|r| r.display().to_string())
                    .unwrap_or_default()
            ),
            "run `by setup`",
        ),
        Store::Withheld(why) => {
            Check::warn(NAME, format!("not opened: {why}"), "see repository.schema")
        }
        Store::Failed(why) => Check::fail(
            NAME,
            format!("the store did not open: {why}"),
            "fix the cause named; the state is .branchyard/state.db (docs/durability.md)",
        ),
    }]
}

/// The store's schema version against this binary's.
pub(super) fn repository_schema(ctx: &Context) -> Vec<Check> {
    const NAME: &str = "repository.schema";
    let Some(root) = ctx.root() else {
        return vec![skipped(NAME, "there is no repository", "repository.git")];
    };
    let supported = doctor::SUPPORTED_SCHEMA;
    vec![match doctor::store_schema(root) {
        Err(error) => Check::fail(
            NAME,
            error.to_string(),
            "restore .branchyard/state.db from a backup; it is a SQLite file Branchyard wrote",
        ),
        Ok(None) => Check::ok(
            NAME,
            format!(
                "no .branchyard/state.db yet; the first command that opens the repository \
                 creates it at schema {supported}"
            ),
        ),
        Ok(Some(schema)) => {
            let found = schema.found.unwrap_or_default();
            match schema.state() {
                SchemaState::Current => Check::ok(
                    NAME,
                    format!("schema {found}, the version this binary reads"),
                ),
                SchemaState::Unrecorded => Check::ok(
                    NAME,
                    format!("state.db records no schema yet; the first open records {supported}"),
                ),
                SchemaState::Upgradable => Check::warn(
                    NAME,
                    format!(
                        "schema {found}; this binary reads {supported} and upgrades the store in \
                         place the first time any `by` command opens it, after which an older `by` \
                         cannot read it"
                    ),
                    "back up .branchyard/state.db if you may go back to an older by, then run any \
                     by command",
                ),
                SchemaState::Newer => Check::fail(
                    NAME,
                    format!(
                        "schema {found} was written by a newer by; this binary reads {supported}"
                    ),
                    "use the newer by, or install it over this one (install.sh)",
                ),
                SchemaState::Unsupported => Check::fail(
                    NAME,
                    format!(
                        "schema {found} is not one this binary reads or upgrades ({supported})"
                    ),
                    "use the by that wrote .branchyard/state.db, or restore it from a backup",
                ),
            }
        }
    }]
}

/// Services whose lease ran out or whose owner is gone.
pub(super) fn repository_services(ctx: &Context) -> Vec<Check> {
    const NAME: &str = "repository.services";
    vec![match ctx.store() {
        Store::Open(yard) => services_row(yard, now_ms()),
        Store::NoRepository => skipped(NAME, "there is no repository", "repository.git"),
        Store::Absent => Check::ok(NAME, "no .branchyard/ yet, so nothing is registered"),
        Store::Withheld(_) | Store::Failed(_) => {
            skipped(NAME, "the store is not open", "repository.state")
        }
    }]
}

fn services_row(yard: &Yard, now: u64) -> Check {
    const NAME: &str = "repository.services";
    if !yard.has_services() {
        return Check::ok(NAME, "no registry yet: nothing has registered a service");
    }
    let services = match yard.services() {
        Ok(registry) => match ServiceStore::all(&*registry) {
            Ok(services) => services,
            Err(error) => return unreadable_registry(&error.to_string()),
        },
        Err(error) => return unreadable_registry(&error.to_string()),
    };
    let stale = stale_services(&services, now);
    if stale.is_empty() {
        let live = services.iter().filter(|s| s.live_at(now)).count();
        return Check::ok(NAME, format!("no expired lease; {live} live"));
    }
    let named: Vec<String> = stale
        .iter()
        .map(|s| format!("{} {}", s.kind, s.id))
        .collect();
    Check::warn(
        NAME,
        format!(
            "{} service{} expired or lapsed: {}",
            stale.len(),
            if stale.len() == 1 { "" } else { "s" },
            named.join(", ")
        ),
        "`by services gc` reclaims what they left behind",
    )
}

fn unreadable_registry(error: &str) -> Check {
    Check::fail(
        "repository.services",
        format!("the registry cannot be read: {error}"),
        "the registry is .branchyard/registry.db (docs/registry.md)",
    )
}

/// The services that expired or lapsed, as `by services` shows them.
fn stale_services(services: &[Service], now: u64) -> Vec<&Service> {
    services
        .iter()
        .filter(|s| match s.state {
            ServiceState::Expired | ServiceState::Reclaiming => true,
            ServiceState::Live => !s.live_at(now),
            ServiceState::Left | ServiceState::Reclaimed => false,
        })
        .collect()
}

// Configuration.

/// The user file loads and validates.
pub(super) fn config_user(ctx: &Context) -> Vec<Check> {
    let c = &ctx.configuration;
    let none = format!("no user file at {} (optional)", c.located.user.display());
    vec![layer_row("config.user", &c.user, &c.located.user, &none)]
}

/// The project file loads and validates.
pub(super) fn config_project(ctx: &Context) -> Vec<Check> {
    let c = &ctx.configuration;
    let none = format!(
        "no branchyard.toml at or above {}; `by init project` writes one",
        ctx.base.display()
    );
    vec![layer_row(
        "config.project",
        &c.project,
        &c.located.project,
        &none,
    )]
}

fn layer_row(name: &'static str, layer: &Layer, path: &Path, none: &str) -> Check {
    match layer {
        Layer::Absent => Check::ok(name, none),
        Layer::Loaded => Check::ok(name, format!("{} loads and validates", path.display())),
        Layer::Refused(why) => Check::fail(
            name,
            why,
            "fix the line it names, then check with `by config validate`",
        ),
    }
}

/// The two files merge with the `BRANCHYARD_*` variables.
pub(super) fn config_merged(ctx: &Context) -> Vec<Check> {
    const NAME: &str = "config.merged";
    vec![match &ctx.configuration.merged {
        Merged::Ready(_) => Check::ok(NAME, "the files merge with the BRANCHYARD_* variables"),
        Merged::Blocked => skipped(
            NAME,
            "a configuration file does not load",
            "config.user or config.project",
        ),
        Merged::Refused(why) => Check::fail(
            NAME,
            why,
            "fix the key it names, or unset the BRANCHYARD_* variable it came from",
        ),
    }]
}

/// `[workspace]` scripts: trusted, and trusted as they are now.
pub(super) fn config_workspace_trust(ctx: &Context) -> Vec<Check> {
    const NAME: &str = "config.workspace_trust";
    let Some(root) = ctx.root() else {
        return vec![skipped(NAME, "there is no repository", "repository.git")];
    };
    if ctx.config().is_none() {
        return vec![skipped(
            NAME,
            "the configuration does not load",
            "config.merged",
        )];
    }
    vec![match workspace_cmd::resolve(root) {
        Ok(None) => Check::ok(
            NAME,
            "no [workspace]: new worktrees get nothing copied and run no setup",
        ),
        Ok(Some(resolved)) => trust_row(&resolved),
        Err(error) => Check::warn(
            NAME,
            format!("{NOT_CHECKED}{}", first_line(&error.to_string())),
            "see config.user, config.project, and the trust file named",
        ),
    }]
}

fn trust_row(resolved: &workspace_cmd::Resolved) -> Check {
    const NAME: &str = "config.workspace_trust";
    let file = resolved.file.display();
    let digest = short(&resolved.digest);
    match resolved.trust {
        TrustState::NotNeeded => Check::ok(
            NAME,
            format!("[workspace] in {file} needs no trust: it runs no commands, or it is yours"),
        ),
        TrustState::Trusted => Check::ok(
            NAME,
            format!(
                "[workspace] in {file} is trusted; its digest {digest} matches the one you trusted"
            ),
        ),
        TrustState::Untrusted => Check::warn(
            NAME,
            format!(
                "[workspace] in {file} has scripts you have not trusted (digest {digest}); new \
                 branches are refused until you do"
            ),
            "review it with `by workspace show`, then run `by workspace trust`",
        ),
        TrustState::Changed => {
            let before = workspace_cmd::trusted_digest(
                workspace_cmd::TrustSection::Repositories,
                &resolved.root.display().to_string(),
            )
            .ok()
            .flatten()
            .map_or_else(String::new, |d| format!("trusted digest {}, ", short(&d)));
            Check::warn(
                NAME,
                format!(
                    "[workspace] in {file} changed since you trusted it ({before}now {digest}); \
                     new branches are refused until you trust it again"
                ),
                "review it with `by workspace show`, then run `by workspace trust`",
            )
        }
    }
}

// Harnesses.

/// What `by harnesses` detects, as a summary and one row for each installed
/// harness: version, login and live run in one short line each.
pub(super) fn harnesses_installed(ctx: &Context) -> Vec<Check> {
    if ctx
        .config()
        .is_some_and(|config| config.defaults.provider == Some(ConfiguredProvider::Substrate))
    {
        return vec![Check::ok("harnesses.summary",
            "harnesses run in the Substrate image; host binaries and host logins are not required or verified")];
    }
    match ctx.inventory() {
        Ok(inventory) => harness_rows(&inventory.harnesses, &branchyard::harnesses()),
        Err(error) => vec![Check::warn(
            "harnesses.summary",
            format!("detection failed: {error}"),
            "run `by harnesses --refresh` to see it",
        )],
    }
}

/// `harnesses.summary`, then `harnesses.ID` for each installed harness, in
/// catalog order.
fn harness_rows(installed: &[HarnessState], known: &[HarnessInfo]) -> Vec<Check> {
    let mut rows = vec![harness_summary(installed)];
    rows.extend(installed.iter().map(|state| harness_row(state, known)));
    rows
}

/// How many are installed, and how many can be used: ok when at least one
/// can run.
fn harness_summary(installed: &[HarnessState]) -> Check {
    const NAME: &str = "harnesses.summary";
    if installed.is_empty() {
        return Check::warn(
            NAME,
            "no known harness is installed on this machine",
            "`by harnesses --all` lists them with install commands; `by harnesses install ID` \
             installs one",
        );
    }
    let (mut logged_in, mut unknown, mut unverified, mut logged_out, mut blocked) = (0, 0, 0, 0, 0);
    for state in installed {
        match (state.standing(), state.login.state) {
            (Standing::Ready, LoginState::Unknown) => unknown += 1,
            (Standing::Ready, _) => logged_in += 1,
            (Standing::NotVerified(_), _) => unverified += 1,
            (Standing::CannotRun(_), LoginState::LoggedOut) => logged_out += 1,
            (Standing::CannotRun(_), _) => blocked += 1,
        }
    }
    let parts: Vec<String> = [
        (logged_in, "logged in"),
        (unknown, "login unknown"),
        (unverified, "login not verified"),
        (logged_out, "logged out"),
        (blocked, "cannot run"),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, what)| format!("{n} {what}"))
    .collect();
    let detail = format!("{} installed: {}", installed.len(), parts.join(", "));
    match logged_in + unknown {
        // Where everyone starts: log in to one.
        0 => Check::todo(
            NAME,
            detail,
            "log in to one: see the harnesses.* rows, or `by harnesses` for the table",
        ),
        _ => Check::ok(NAME, detail),
    }
}

/// One installed harness: `VERSION · LOGIN · LIVE RUN` in a line of at most
/// 120 characters. The version is named `installed X` beside the version
/// its live run was on when the two differ: a live run says nothing about
/// another version.
fn harness_row(state: &HarnessState, known: &[HarnessInfo]) -> Check {
    let name = format!("harnesses.{}", state.id);
    let qualification = crate::harness_cmd::qualification_of(known, &state.id);
    let q = qualification.as_deref();
    let drift = inventory::live_drift(state.version.as_deref(), q).is_some();
    let version = match (&state.version, drift) {
        (Some(v), true) => format!("installed {v}"),
        (Some(v), false) => v.clone(),
        (None, _) => match state.version_text().as_str() {
            "?" => "version unknown".to_owned(),
            slow => slow.to_owned(),
        },
    };
    let login = match state.standing() {
        Standing::NotVerified(_) => "login not verified".to_owned(),
        _ => state.login.label(),
    };
    let live = match (q.and_then(inventory::live_score), drift) {
        (Some(score), true) => format!(
            "live-tested {} ({score})",
            q.and_then(inventory::live_version).unwrap_or_default()
        ),
        (Some(score), false) => format!("live {score}"),
        (None, _) => "no live run".to_owned(),
    };
    let detail = format!("{version} · {login} · {live}");
    match state.standing() {
        // The key is set but agy will not read it: a step to take.
        Standing::Ready if state.login.key_unused() => Check::todo(
            name,
            detail,
            "run `by harnesses login antigravity`: GEMINI_API_KEY is set but agy reads it only \
             with modelProvider gemini in its settings",
        ),
        Standing::Ready => Check::ok(name, detail),
        // A login not made yet is a step to take, not a fault.
        Standing::NotVerified(_) => Check::todo(name, detail, state.login_hint()),
        Standing::CannotRun(_) if state.login.state == LoginState::LoggedOut => {
            Check::todo(name, detail, state.login_hint())
        }
        Standing::CannotRun(why) => Check::warn(name, detail, why),
    }
}

/// `[defaults] harness`: set, and installed.
pub(super) fn harnesses_default(ctx: &Context) -> Vec<Check> {
    const NAME: &str = "harnesses.default";
    let Some(config) = ctx.config() else {
        return vec![skipped(
            NAME,
            "the configuration does not load",
            "config.merged",
        )];
    };
    if config.defaults.provider == Some(ConfiguredProvider::Substrate) {
        let machine = branchyard::up::LocalMachine::new();
        let root = ctx.root().unwrap_or(&ctx.base);
        return branchyard::up::remote_harness_readiness(root, config, &machine)
            .into_iter()
            .map(provider_check)
            .collect();
    }
    if matches!(
        config.defaults.provider,
        Some(ConfiguredProvider::Container | ConfiguredProvider::AgentSandbox)
    ) {
        return vec![Check::warn(
            NAME,
            "the native image supplies the harness; image execution and model login were not checked",
            "qualify the image's selected harness and provision its model credentials on the worker",
        )];
    }
    let set = config.defaults.harness.as_deref();
    let installed = ctx
        .inventory()
        .as_ref()
        .ok()
        .map(|i| i.harnesses.as_slice());
    vec![default_harness_row(
        set,
        &branchyard::harnesses(),
        installed,
    )]
}

/// `[defaults] harness`: known, installed, and, where the inventory says,
/// not known to be logged out. `installed` is what detection found, if it
/// ran.
fn default_harness_row(
    set: Option<&str>,
    known: &[HarnessInfo],
    installed: Option<&[HarnessState]>,
) -> Check {
    const NAME: &str = "harnesses.default";
    let id = set.unwrap_or(BUILT_IN_HARNESS);
    let matching: Vec<&HarnessInfo> = known
        .iter()
        .filter(|k| k.harness == id || k.profile == id)
        .collect();
    let on_path = matching.iter().any(|k| k.available);
    // Installed, but nothing says it can be used: the same words
    // `harnesses.ID` uses for it.
    let standing = matching
        .first()
        .and_then(|k| installed?.iter().find(|s| s.id == k.harness))
        .map(|state| (state.standing(), state.login_hint()));
    match (&standing, on_path) {
        (Some((Standing::NotVerified(_), hint)), true) => {
            return Check::warn(
                NAME,
                format!(
                    "{} {id}, which is installed, but its login is not verified",
                    match set {
                        Some(_) => "[defaults] harness =",
                        None => "not set: `by run` without --harness uses",
                    }
                ),
                hint.clone(),
            )
        }
        (Some((Standing::CannotRun(why), hint)), true) => {
            return Check::warn(
                NAME,
                format!(
                    "{} {id}, which is installed, but cannot run: {}",
                    match set {
                        Some(_) => "[defaults] harness =",
                        None => "not set: `by run` without --harness uses",
                    },
                    first_line(why)
                ),
                hint.clone(),
            )
        }
        _ => {}
    }
    let installed = on_path;
    match (set, matching.is_empty(), installed) {
        (Some(id), true, _) => Check::fail(
            NAME,
            format!("[defaults] harness = {id:?} is not a harness or profile Branchyard knows"),
            "`by harnesses --all` lists them; change [defaults] harness in branchyard.toml",
        ),
        (Some(id), false, false) => Check::fail(
            NAME,
            format!("[defaults] harness = {id:?} is not installed, so `by run` would fail"),
            format!("`by harnesses install {id}`, or change [defaults] harness"),
        ),
        (Some(id), false, true) => {
            Check::ok(NAME, format!("[defaults] harness = {id:?}, and it is installed"))
        }
        (None, _, true) => Check::ok(
            NAME,
            format!("not set: `by run` without --harness uses {id}, which is installed"),
        ),
        (None, _, false) => Check::warn(
            NAME,
            format!("not set, and the built-in default {id} is not installed"),
            "set harness under [defaults] in branchyard.toml (`by init project`), or pass --harness",
        ),
    }
}

/// Credential variables, by name only, and the sources `[secrets]` names.
pub(super) fn harnesses_credentials(ctx: &Context) -> Vec<Check> {
    const NAME: &str = "harnesses.credentials";
    let set: Vec<&str> = KNOWN_VARIABLES
        .iter()
        .copied()
        .filter(|name| !name.starts_with("BRANCHYARD_") && ctx.var_set(name))
        .collect();
    let mut detail = match set.is_empty() {
        true => "none of the usual credential variables is set (harnesses may use their own \
                 logins)"
            .to_owned(),
        false => format!("set: {} (names only)", set.join(", ")),
    };
    let root = ctx.root().unwrap_or(&ctx.base);
    let missing = ctx
        .config()
        .map(|config| missing_secrets(root, &config.secrets, &|name| ctx.var_set(name)))
        .unwrap_or_default();
    // Provider logins, and any whose key `[secrets]` does not name.
    let (logins, keyless) = ctx.config().map(provider_logins).unwrap_or_default();
    if !logins.is_empty() {
        detail.push_str(&format!("; [logins]: {}", logins.join(", ")));
    }
    if missing.is_empty() && keyless.is_empty() {
        return vec![Check::ok(NAME, detail)];
    }
    if !missing.is_empty() {
        detail.push_str(&format!(
            "; [secrets] without a source here: {}",
            missing.join(", ")
        ));
    }
    if !keyless.is_empty() {
        detail.push_str(&format!(
            "; [logins] whose key [secrets] does not name: {}",
            keyless.join(", ")
        ));
    }
    vec![Check::warn(
        NAME,
        detail,
        match keyless.first() {
            Some(first) => format!(
                "store the provider's key with `by harnesses login {}`",
                first.replace(" via ", " --provider ")
            ),
            None => "export the variable (or create the file) each secret names; isolated and \
                     sandboxed branches are given them"
                .to_owned(),
        },
    )]
}

/// `[logins]` as `ID via PROVIDER (KEY)`, and those whose key `[secrets]`
/// does not name as `ID via PROVIDER`, which an isolated run would refuse.
fn provider_logins(config: &branchyard_setup::config::ProjectConfig) -> (Vec<String>, Vec<String>) {
    let mut logins = Vec::new();
    let mut keyless = Vec::new();
    for (id, provider) in &config.logins {
        let Some(login) = branchyard::logins::find(id, provider) else {
            continue;
        };
        logins.push(format!("{id} via {provider} ({})", login.key));
        if !config.secrets.contains_key(login.key) {
            keyless.push(format!("{id} via {provider}"));
        }
    }
    (logins, keyless)
}

/// The variables that point harnesses at Vertex AI or name its project and
/// region: Claude Code's, Gemini CLI's, Antigravity's ADC switch, Hermes's,
/// and the projects OpenCode reads.
const VERTEX_VARIABLES: &[&str] = &[
    "CLAUDE_CODE_USE_VERTEX",
    "ANTHROPIC_VERTEX_PROJECT_ID",
    "CLOUD_ML_REGION",
    "GOOGLE_GENAI_USE_VERTEXAI",
    "GOOGLE_CLOUD_PROJECT",
    "GOOGLE_CLOUD_LOCATION",
    "GCP_PROJECT",
    "GCLOUD_PROJECT",
    "AGY_ADC_AUTH",
    "VERTEX_PROJECT_ID",
    "VERTEX_REGION",
];

/// Where Application Default Credentials come from on this machine, in the
/// order Google's client libraries look for them.
#[derive(Debug, PartialEq)]
enum AdcSource {
    /// `GOOGLE_APPLICATION_CREDENTIALS` names a file that exists.
    KeyFile,
    /// `GOOGLE_APPLICATION_CREDENTIALS` names a file that does not.
    MissingKeyFile(String),
    /// gcloud's file, as shown to people.
    Gcloud(String),
    /// A Google Compute Engine VM's metadata server.
    Metadata,
    /// None of these.
    None,
}

/// What `harnesses.google_cloud` reads, so the decision is tested without
/// this machine's files.
struct GoogleCloud<'a> {
    var: &'a dyn Fn(&str) -> Option<String>,
    exists: &'a dyn Fn(&Path) -> bool,
    /// `/sys/class/dmi/id/product_name`, when it can be read.
    product: Option<String>,
}

impl GoogleCloud<'_> {
    fn set(&self, name: &str) -> Option<String> {
        (self.var)(name).filter(|v| !v.is_empty())
    }

    /// gcloud's ADC file, as `(path, shown)`: under `CLOUDSDK_CONFIG`, else
    /// `~/.config/gcloud`.
    fn gcloud_file(&self) -> Option<(std::path::PathBuf, String)> {
        const NAME: &str = "application_default_credentials.json";
        match self.set("CLOUDSDK_CONFIG") {
            Some(dir) => Some((Path::new(&dir).join(NAME), format!("{dir}/{NAME}"))),
            None => self.set("HOME").map(|home| {
                (
                    Path::new(&home).join(".config/gcloud").join(NAME),
                    format!("~/.config/gcloud/{NAME}"),
                )
            }),
        }
    }

    fn source(&self) -> AdcSource {
        if let Some(path) = self.set("GOOGLE_APPLICATION_CREDENTIALS") {
            return match (self.exists)(Path::new(&path)) {
                true => AdcSource::KeyFile,
                false => AdcSource::MissingKeyFile(path),
            };
        }
        if let Some((path, shown)) = self.gcloud_file() {
            if (self.exists)(&path) {
                return AdcSource::Gcloud(shown);
            }
        }
        match self.product.as_deref() {
            Some(product) if product.contains("Google") => AdcSource::Metadata,
            _ => AdcSource::None,
        }
    }

    /// Whether a harness is switched to Vertex AI or ADC: Claude Code's
    /// switch is true, Gemini CLI's or Antigravity's is exactly `true`, or a
    /// project is set, which turns OpenCode's and Hermes's Vertex AI on and
    /// which Gemini CLI's Vertex AI needs (however its settings select it).
    fn vertex_in_use(&self) -> bool {
        let claude = self.set("CLAUDE_CODE_USE_VERTEX").is_some_and(|v| {
            ["1", "true", "yes", "on"].contains(&v.trim().to_lowercase().as_str())
        });
        let gemini = self.set("GOOGLE_GENAI_USE_VERTEXAI").as_deref() == Some("true");
        let antigravity = self.set("AGY_ADC_AUTH").as_deref() == Some("true");
        let projects = [
            "GOOGLE_CLOUD_PROJECT",
            "GCP_PROJECT",
            "GCLOUD_PROJECT",
            "VERTEX_PROJECT_ID",
        ];
        let project = projects.iter().any(|name| self.set(name).is_some());
        claude || gemini || antigravity || project
    }

    /// The row, or none when no Vertex AI variable, key file or gcloud file
    /// is there: a Google VM alone is a source, not a reason to report.
    fn check(&self) -> Option<Check> {
        const NAME: &str = "harnesses.google_cloud";
        const FIX: &str = "run `gcloud auth application-default login` (not `gcloud auth \
                           login`), or set GOOGLE_APPLICATION_CREDENTIALS to a service account \
                           key file (docs/setup.md#google-cloud)";
        let set: Vec<&str> = VERTEX_VARIABLES
            .iter()
            .copied()
            .filter(|name| self.set(name).is_some())
            .collect();
        let variables = match set.is_empty() {
            true => "no Vertex AI variable is set".to_owned(),
            false => format!("Vertex AI variables set: {} (names only)", set.join(", ")),
        };
        let source = self.source();
        let gcloud_file = self
            .gcloud_file()
            .is_some_and(|(path, _)| (self.exists)(&path));
        if set.is_empty() && self.set("GOOGLE_APPLICATION_CREDENTIALS").is_none() && !gcloud_file {
            return None;
        }
        let adc = |from: &str| format!("Application Default Credentials: {from}; {variables}");
        Some(match source {
            AdcSource::KeyFile => Check::ok(
                NAME,
                adc("the file GOOGLE_APPLICATION_CREDENTIALS names (not read)"),
            ),
            AdcSource::MissingKeyFile(path) => Check::warn(
                NAME,
                format!("GOOGLE_APPLICATION_CREDENTIALS names {path}, which does not exist; {variables}"),
                "point GOOGLE_APPLICATION_CREDENTIALS at a credentials file, or unset it",
            ),
            AdcSource::Gcloud(shown) => Check::ok(
                NAME,
                adc(&format!(
                    "{shown}, from `gcloud auth application-default login` (isolated and \
                     sandboxed branches get a copy with [secrets] GCLOUD_ADC = \"@{shown}\")"
                )),
            ),
            AdcSource::Metadata => Check::ok(
                NAME,
                adc(
                    "the metadata server's service account, on this Google Compute Engine VM \
                     (its scopes are not checked: Vertex AI needs cloud-platform)",
                ),
            ),
            AdcSource::None if self.vertex_in_use() => Check::todo(
                NAME,
                format!(
                    "{variables}, but no Application Default Credentials were found (no \
                     GOOGLE_APPLICATION_CREDENTIALS, no {}, and this is not a Google Compute \
                     Engine VM)",
                    self.gcloud_file()
                        .map_or("gcloud file".to_owned(), |(_, shown)| shown)
                ),
                FIX,
            ),
            AdcSource::None => Check::ok(
                NAME,
                format!(
                    "{variables}; no harness is switched to Vertex AI and no Application \
                     Default Credentials were found"
                ),
            ),
        })
    }
}

/// Application Default Credentials and the Vertex AI variables, by name,
/// for a machine on Google Cloud or a harness pointed at Vertex AI.
pub(super) fn harnesses_google_cloud(ctx: &Context) -> Vec<Check> {
    // No such file is the usual answer off Linux and in many containers:
    // not a Google VM, as far as this row can tell.
    let product = match std::fs::read_to_string("/sys/class/dmi/id/product_name") {
        Ok(text) => Some(text.trim().to_owned()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => branchyard_support::best_effort(
            "read the DMI product name for harnesses.google_cloud",
            Err::<String, _>(error),
        ),
    };
    GoogleCloud {
        var: &|name| (ctx.vars)(name),
        exists: &|path| path.exists(),
        product,
    }
    .check()
    .into_iter()
    .collect()
}

/// The `[secrets]` whose variable is unset or whose file is missing, as
/// `NAME (from $VAR)` or `NAME (from @path)`.
fn missing_secrets(
    root: &Path,
    secrets: &BTreeMap<String, String>,
    var_set: &dyn Fn(&str) -> bool,
) -> Vec<String> {
    use branchyard::models::KeySource;
    secrets
        .keys()
        .filter_map(|name| {
            let source = crate::models_cmd::key_source(root, secrets, name);
            let present = match &source {
                KeySource::Env(var) => var_set(var),
                KeySource::File(path) => path.is_file(),
            };
            (!present).then(|| format!("{name} (from {source})"))
        })
        .collect()
}

// Providers.

/// The provider `[defaults] provider` names, as the engine would check it.
pub(super) fn provider_default(ctx: &Context) -> Vec<Check> {
    const NAME: &str = "provider.default";
    let Some(config) = ctx.config() else {
        return vec![skipped(
            NAME,
            "the configuration does not load",
            "config.merged",
        )];
    };
    vec![match config
        .defaults
        .provider
        .unwrap_or(ConfiguredProvider::Local)
    {
        ConfiguredProvider::Local => Check::ok(
            NAME,
            "local (the default): harnesses run as your user in a worktree, with no isolation",
        ),
        ConfiguredProvider::Microsandbox => microsandbox_row(ctx, config),
        ConfiguredProvider::Container | ConfiguredProvider::AgentSandbox => Check::warn(
            NAME,
            "native sandbox configuration loaded; runtime, image, worker capacity and workload isolation were not checked",
            "qualify the selected provider on its worker before running a workload",
        ),
        ConfiguredProvider::Substrate => provider_check(ctx.substrate().step()),
        ConfiguredProvider::Recipe => recipe_row(ctx, config),
    }]
}

/// Keep provider and remote credential findings identical to `by up`.
fn provider_check(step: branchyard::up::Step) -> Check {
    let name = if step.name == "provider" {
        "provider.default".to_owned()
    } else {
        format!("harnesses.remote.{}", step.name)
    };
    match step.state {
        branchyard::up::State::Ready => Check::ok(name, step.detail),
        branchyard::up::State::Blocked => {
            Check::fail(name, step.detail, step.todo.unwrap_or_default())
        }
        _ => Check::warn(name, step.detail, step.todo.unwrap_or_default()),
    }
}

fn microsandbox_row(ctx: &Context, config: &ProjectConfig) -> Check {
    const NAME: &str = "provider.default";
    let Some(options) = config.microsandbox.as_ref().map(sandbox_options) else {
        return Check::fail(
            NAME,
            "[defaults] provider = \"microsandbox\", but there is no [microsandbox] section",
            "add [microsandbox] with an image to branchyard.toml (docs/providers.md)",
        );
    };
    match ctx.store() {
        Store::Open(yard) => match doctor::check_microsandbox(yard, &options) {
            Ok(()) => match doctor::check_microsandbox_host() {
                Ok(()) => Check::ok(NAME, format!(
                    "microsandbox with image {}: build support and host KVM access checked; VM startup unverified", options.image)),
                Err(error) => Check::fail(NAME, error,
                    "fix /dev/kvm availability and permissions, then rerun `by up` (docs/providers.md)"),
            },
            Err(error) => Check::fail(
                NAME,
                error.to_string(),
                format!(
                    "rebuild with `{}` (needs KVM and libcap-ng) and give [microsandbox] an \
                     image (docs/modes.md#sandbox)",
                    branchyard::MICROSANDBOX_REBUILD
                ),
            ),
        },
        _ => skipped(
            NAME,
            "the engine's check needs the open store",
            "repository.state",
        ),
    }
}

/// The engine's options for `[microsandbox]`, as `by run` builds them.
fn sandbox_options(section: &Microsandbox) -> SandboxOptions {
    SandboxOptions {
        image: section.image.clone(),
        cpus: section.cpus,
        memory_mib: section.memory_mib,
        pass_env: section.pass_env.clone(),
        keep: match section.keep.as_deref() {
            Some("pause") => SandboxKeep::Pause,
            _ => SandboxKeep::Destroy,
        },
        snapshots: section.snapshots,
        max_paused: section.max_paused,
        max_kept_secs: section.max_kept_minutes.map(|m| u64::from(m) * 60),
        live_branch: section.live_branch.unwrap_or(false),
    }
}

fn recipe_row(ctx: &Context, config: &ProjectConfig) -> Check {
    const NAME: &str = "provider.default";
    let Some(name) = config.defaults.recipe.as_deref() else {
        return Check::fail(
            NAME,
            "[defaults] provider = \"recipe\", but [defaults] recipe names none",
            "set recipe under [defaults] to one of your [recipes.NAME]",
        );
    };
    let Some(root) = ctx.root() else {
        return skipped(NAME, "there is no repository", "repository.git");
    };
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    match crate::recipe_cmd::readiness(&root, name) {
        Ok(crate::recipe_cmd::Readiness::Runs) => Check::ok(
            NAME,
            format!(
                "recipe {name} is defined and may run; `by recipe check {name}` runs its doctor \
                 and a smoke test, which doctor does not"
            ),
        ),
        Ok(crate::recipe_cmd::Readiness::Undefined) => Check::fail(
            NAME,
            format!("[defaults] recipe = {name:?}, but there is no [recipes.{name}]"),
            "define the recipe in branchyard.toml or your user file (docs/recipes.md)",
        ),
        Ok(crate::recipe_cmd::Readiness::Untrusted | crate::recipe_cmd::Readiness::Changed) => {
            Check::warn(
                NAME,
                format!("recipe {name} has commands you have not trusted as they are now"),
                format!("review `by recipe show {name}`, then `by recipe trust {name}`"),
            )
        }
        Err(error) => Check::warn(
            NAME,
            format!("{NOT_CHECKED}{}", first_line(&error.to_string())),
            "see config.project",
        ),
    }
}

/// Kept sandboxes and snapshots, and their age against `max_kept_minutes`.
pub(super) fn provider_sandboxes(ctx: &Context) -> Vec<Check> {
    const NAME: &str = "provider.sandboxes";
    let Some(config) = ctx.config() else {
        return vec![skipped(
            NAME,
            "the configuration does not load",
            "config.merged",
        )];
    };
    let (Some(ConfiguredProvider::Microsandbox), Some(section)) =
        (config.defaults.provider, config.microsandbox.as_ref())
    else {
        return vec![Check::ok(
            NAME,
            "no sandbox provider is configured, so no sandbox is kept",
        )];
    };
    vec![match ctx.store() {
        Store::Open(yard) => match doctor::held_sandboxes(yard, &sandbox_options(section)) {
            Ok(held) => sandboxes_row(&held, section.max_kept_minutes, now_ms()),
            Err(error) => Check::fail(
                NAME,
                format!("the store's sandbox rows cannot be read: {error}"),
                "see repository.state",
            ),
        },
        Store::Absent => Check::ok(NAME, "no .branchyard/ yet, so none is kept"),
        _ => skipped(NAME, "the store is not open", "repository.state"),
    }]
}

/// Kept sandboxes and snapshots as one row. A kept sandbox idle past
/// `max_kept_minutes` is warned about: nothing destroys it until a turn
/// ends on the provider or its branch takes its next turn.
fn sandboxes_row(held: &[HeldSandbox], max_kept_minutes: Option<u32>, now: u64) -> Check {
    const NAME: &str = "provider.sandboxes";
    let kept: Vec<&HeldSandbox> = held.iter().filter(|h| h.kind == HeldKind::Kept).collect();
    let snapshots = held.len() - kept.len();
    let age_secs = |h: &HeldSandbox| now.saturating_sub(h.used_ms) / 1000;
    let mut detail = format!(
        "{} kept sandbox{}, {} snapshot{}",
        kept.len(),
        if kept.len() == 1 { "" } else { "es" },
        snapshots,
        if snapshots == 1 { "" } else { "s" }
    );
    if let Some(oldest) = kept.iter().map(|h| age_secs(h)).max() {
        detail.push_str(&format!("; the oldest unused for {}", age_text(oldest)));
    }
    let Some(limit) = max_kept_minutes else {
        detail.push_str("; no max_kept_minutes, so they stay until max_paused evicts them");
        return Check::ok(NAME, detail);
    };
    let over: Vec<String> = kept
        .iter()
        .filter(|h| age_secs(h) > u64::from(limit) * 60)
        .map(|h| format!("{} ({})", h.name, h.branch))
        .collect();
    if over.is_empty() {
        detail.push_str(&format!("; none over max_kept_minutes = {limit}"));
        return Check::ok(NAME, detail);
    }
    Check::warn(
        NAME,
        format!(
            "{detail}; unused for longer than max_kept_minutes = {limit}: {}",
            over.join(", ")
        ),
        "a turn ending on this provider, or the branch's next turn, destroys them; `by rm BRANCH` \
         destroys one now",
    )
}

// Host.

/// Unprivileged user and network namespaces, which confine a local harness
/// to the egress proxy (docs/egress.md).
pub(super) fn host_namespaces(ctx: &Context) -> Vec<Check> {
    const NAME: &str = "host.namespaces";
    let must_enforce = ctx.config().is_some_and(|config| {
        config.network.allow.is_some()
            && config.network.enforce == Some(NetworkEnforceMode::Required)
    });
    vec![match doctor::egress_confinement() {
        Ok(()) => Check::ok(
            NAME,
            "unprivileged user and network namespaces work: a local harness can be confined to \
             the egress proxy",
        ),
        Err(why) if must_enforce => Check::fail(
            NAME,
            format!(
                "[network] enforce = \"required\", but this host cannot confine a local harness: \
                 {why}"
            ),
            "allow unprivileged user namespaces (the kernel setting or the security module named), \
             or use enforce = \"best_effort\" (docs/egress.md)",
        ),
        Err(why) => Check::warn(
            NAME,
            format!("this host cannot confine a local harness: {why}"),
            "a network policy would be advisory only; allow unprivileged user namespaces to \
             enforce it (docs/egress.md)",
        ),
    }]
}

/// Landlock, which holds a local harness to its permission preset's file
/// scope (docs/modes.md#the-file-guard).
pub(super) fn host_landlock(_: &Context) -> Vec<Check> {
    const NAME: &str = "host.landlock";
    vec![match branchyard::guard::support() {
        Ok(abi) => Check::ok(
            NAME,
            format!(
                "Landlock ABI {abi}: under --permissions read-only or edit-worktree a local \
                 harness can write only in its preset's scope"
            ),
        ),
        Err(why) => Check::warn(
            NAME,
            format!("no file guard: {why}"),
            "--permissions read-only and edit-worktree then decide only the tool calls a \
             harness asks about; a Linux 5.13 or newer kernel with Landlock enabled holds the \
             rest (docs/modes.md#the-file-guard)",
        ),
    }]
}

/// A loopback listener, which the egress proxy and the gateways need.
pub(super) fn host_loopback(_: &Context) -> Vec<Check> {
    const NAME: &str = "host.loopback";
    vec![match std::net::TcpListener::bind(("127.0.0.1", 0)) {
        Ok(_) => Check::ok(NAME, "a listener on 127.0.0.1 can be opened"),
        Err(error) => Check::warn(
            NAME,
            format!("cannot listen on 127.0.0.1: {error}"),
            "the egress proxy and the gateways listen on loopback; check the network namespace \
             or sandbox this runs in",
        ),
    }]
}

/// `/dev/kvm`, which `--provider microsandbox` needs.
pub(super) fn host_kvm(ctx: &Context) -> Vec<Check> {
    const NAME: &str = "host.kvm";
    let platform = crate::setup_io::HostProbe::new(&ctx.base).platform();
    vec![if platform.os == "linux" {
        match doctor::check_microsandbox_host() {
            Ok(()) => Check::ok(NAME, "/dev/kvm is open for read/write; VM startup unverified"),
            Err(error) => Check::warn(NAME, error,
                "fix KVM availability or group permissions and start a new login session; local branches need no KVM"),
        }
    } else {
        Check::ok(NAME, format!("{}: /dev/kvm does not apply", platform.os))
    }]
}

// Server.

/// A configured `[remote]` answers `GET /v1/repos` with its token.
pub(super) fn server_reachable(ctx: &Context) -> Vec<Check> {
    vec![ctx.server().0.clone()]
}

/// Ask the configured server once: the `server.reachable` row, and what it
/// answered for the readiness checks (`None` when none is configured).
pub(super) fn ask_server(ctx: &Context) -> (Check, Option<Answer>) {
    const NAME: &str = "server.reachable";
    let globals = &ctx.globals;
    let Some(url) = globals.remote.as_deref() else {
        return (
            Check::ok(
                NAME,
                "not configured: no --remote, BRANCHYARD_REMOTE or [remote] url; commands run here",
            ),
            None,
        );
    };
    // A URL may carry `user:password@`: shown without it.
    let shown = without_userinfo(url);
    if crate::ssh_remote::is_ssh(url) {
        return (
            Check::ok(
                NAME,
                format!(
                    "{shown} is an ssh remote: not checked here; `by remote ssh status` shows it"
                ),
            ),
            Some(Answer::NotAsked(
                "doctor does not contact an ssh remote".into(),
            )),
        );
    }
    if globals.token_file.is_none() {
        let detail = format!("{shown} is configured but no token file is");
        return (
            Check::fail(
                NAME,
                &detail,
                "set token_file under [remote], or pass --token-file / BRANCHYARD_TOKEN_FILE",
            ),
            Some(Answer::Unusable(detail)),
        );
    }
    let client = match client(globals, url) {
        Ok(client) => client,
        Err(error) => {
            return (
                Check::fail(
                    NAME,
                    error.to_string().replace(url, &shown),
                    "check the token file and CA file the remote names",
                ),
                Some(Answer::of_client(&error)),
            )
        }
    };
    let repos = within(SERVER_DEADLINE, move || {
        client
            .repos()
            .map(|repos| repos.into_iter().map(|r| r.name).collect::<Vec<_>>())
    });
    match repos {
        Err(error) => {
            let why = error.as_ref().map_or_else(
                || format!("no answer within {SERVER_DEADLINE:?}"),
                ToString::to_string,
            );
            let answer = match &error {
                Some(error) => Answer::of_client(error),
                None => Answer::Unreachable(why.clone()),
            };
            (
                Check::fail(
                    NAME,
                    format!(
                        "{shown} did not answer GET /v1/repos: {}",
                        why.replace(url, &shown)
                    ),
                    crate::remote::UNREACHABLE_FIX,
                ),
                Some(answer),
            )
        }
        Ok(names) => {
            let wanted = globals.repo.clone();
            let row = match wanted.as_deref() {
                Some(wanted) if !names.iter().any(|n| n == wanted) => Check::fail(
                    NAME,
                    format!(
                        "{shown} answers, but serves {}, not {wanted:?}",
                        list(&names)
                    ),
                    "fix repo under [remote] (or --repo / BRANCHYARD_REPO), or serve it",
                ),
                _ => Check::ok(NAME, format!("{shown} answers; it serves {}", list(&names))),
            };
            (
                row,
                Some(Answer::Repositories {
                    served: names,
                    wanted,
                }),
            )
        }
    }
}

fn list(names: &[String]) -> String {
    match names {
        [] => "no repositories".to_owned(),
        [one] => format!("repository {one}"),
        several => format!("repositories {}", several.join(", ")),
    }
}

/// A client for the configured server at `url`, with the token and CA
/// files `globals` name, whose requests end at the deadline.
fn client(globals: &Globals, url: &str) -> Result<Client, branchyard_client::Error> {
    let token_file = globals
        .token_file
        .as_deref()
        .ok_or_else(|| branchyard_client::Error::Config("no token file is configured".into()))?;
    let client = Client::from_token_file(url, token_file)?;
    let client = match globals.ca_file.as_deref() {
        Some(ca) => client.with_ca_file(ca)?,
        None => client,
    };
    Ok(client.with_timeout(SERVER_DEADLINE))
}

/// What `GET /v1/problems` came to: the list, or the client's error
/// (`None`: no answer within the deadline).
type Listed = Result<ProblemList, Option<branchyard_client::Error>>;

/// What a configured server lists as problems, asked only once its
/// `GET /v1/repos` succeeded.
pub(super) fn server_problems(ctx: &Context) -> Vec<Check> {
    let globals = &ctx.globals;
    let (Some(url), Some(answer)) = (globals.remote.as_deref(), &ctx.server().1) else {
        return Vec::new();
    };
    let list = || {
        let client = client(globals, url).map_err(Some)?;
        within(SERVER_DEADLINE, move || client.problems())
    };
    problems_row(&without_userinfo(url), answer, list)
        .into_iter()
        .collect()
}

/// The `server.problems` row for the server shown as `shown`, from what
/// its `GET /v1/repos` came to (`answer`): none for an ssh remote, which
/// doctor does not contact; `not checked` when that request did not
/// succeed, since `server.reachable` reports the outage; else from what
/// `list` gets from `GET /v1/problems`.
fn problems_row(shown: &str, answer: &Answer, list: impl FnOnce() -> Listed) -> Option<Check> {
    const NAME: &str = "server.problems";
    match answer {
        Answer::NotAsked(_) => return None,
        Answer::Unusable(_) | Answer::Unreachable(_) | Answer::Refused(_) | Answer::Failed(_) => {
            return Some(skipped(
                NAME,
                "GET /v1/repos did not succeed, so GET /v1/problems was not sent",
                "server.reachable",
            ))
        }
        Answer::Repositories { .. } | Answer::Workers { .. } => {}
    }
    Some(match list() {
        Ok(listed) => match listed.problems.first() {
            None => Check::ok(NAME, "no problems"),
            Some(oldest) => {
                let count = match listed.problems.len() {
                    1 => "1 problem".to_owned(),
                    n => format!("{n} problems; the oldest"),
                };
                let detail = format!("{count}: {}: {}", oldest.what, oldest.error);
                Check::warn(NAME, detail, &oldest.next)
            }
        },
        Err(Some(branchyard_client::Error::Api { status: 404, .. })) => Check::warn(
            NAME,
            format!("{shown} has no GET /v1/problems: it is older than this `by`"),
            "upgrade the server to this version of `by` to see what its background work could not do",
        ),
        Err(error) => Check::warn(
            NAME,
            format!(
                "{shown} did not answer GET /v1/problems: {}",
                error.map_or_else(
                    || format!("no answer within {SERVER_DEADLINE:?}"),
                    |error| error.to_string()
                )
            ),
            "check the server's log, then run `by doctor` again",
        ),
    })
}

/// What `ask` gets from the server, or its error (`None`: no answer
/// within `deadline`), whatever the network does: the request runs on a
/// thread of its own, and one that outlives the deadline is left to end
/// with the process.
fn within<T: Send + 'static>(
    deadline: Duration,
    ask: impl FnOnce() -> Result<T, branchyard_client::Error> + Send + 'static,
) -> Result<T, Option<branchyard_client::Error>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        branchyard_support::best_effort("tell doctor what the server answered", sender.send(ask()));
    });
    receiver
        .recv_timeout(deadline)
        .map_err(|_| None)?
        .map_err(Some)
}

// Model gateway.

/// Each `[models]` backend's key is set, by name.
pub(super) fn models_keys(ctx: &Context) -> Vec<Check> {
    const NAME: &str = "models.keys";
    let Some(config) = ctx.config() else {
        return vec![skipped(
            NAME,
            "the configuration does not load",
            "config.merged",
        )];
    };
    if config.models.is_empty() {
        return vec![Check::ok(NAME, "not configured: no [models]")];
    }
    if config.models.backends.is_empty() {
        return vec![Check::warn(
            NAME,
            "[models] names no backend, so no gateway starts",
            "add [models.backends.NAME] with an api and a key (docs/model-gateway.md)",
        )];
    }
    let root = ctx.root().unwrap_or(&ctx.base);
    let rows: Vec<KeyRow> = config
        .models
        .backends
        .iter()
        .map(|(name, backend)| {
            let source = backend
                .key
                .as_deref()
                .map(|key| crate::models_cmd::key_source(root, &config.secrets, key));
            KeyRow {
                backend: name.clone(),
                api: backend.api.clone(),
                source: source.as_ref().map(ToString::to_string),
                present: source.as_ref().map(|s| key_present(s, &|v| ctx.var_set(v))),
                signed: backend.auth.as_deref() == Some("turn-token"),
                plane: backend.control_plane.is_some(),
            }
        })
        .collect();
    let mut checks = vec![keys_row(&rows)];
    checks.extend(transport_row(&config.models));
    checks.extend(audience_row(&config.models));
    checks
}

/// `models.url`: whether a control-plane backend's `url` is plain `http://`
/// on a host that is not loopback ([`ModelsConfig::transport_notes`]), where
/// its credential travels unencrypted unless the network encrypts it. One
/// row for every control-plane backend, and none without one.
///
/// [`ModelsConfig::transport_notes`]: branchyard_setup::config::ModelsConfig::transport_notes
fn transport_row(models: &branchyard_setup::config::ModelsConfig) -> Option<Check> {
    const NAME: &str = "models.url";
    let planes: Vec<&str> = models
        .backends
        .iter()
        .filter(|(_, backend)| backend.control_plane.is_some())
        .map(|(name, _)| name.as_str())
        .collect();
    if planes.is_empty() {
        return None;
    }
    let notes = models.transport_notes();
    Some(match notes.is_empty() {
        true => Check::ok(
            NAME,
            format!(
                "every control-plane backend's url is https, or http on this machine ({})",
                planes.join(", ")
            ),
        ),
        false => Check::warn(
            NAME,
            notes.join("; "),
            "use an https:// url, or make sure the network between Branchyard and AIPlex \
             encrypts what it carries (docs/model-gateway.md#configuring-it)",
        ),
    })
}

/// `models.audience`: whether a signed token's audience names another host
/// than its backend's `url` ([`ModelsConfig::audience_notes`]), where the
/// token is sent. One row for every backend called with a token the yard
/// signs (`auth = "turn-token"`), and none without such a backend.
///
/// [`ModelsConfig::audience_notes`]: branchyard_setup::config::ModelsConfig::audience_notes
fn audience_row(models: &branchyard_setup::config::ModelsConfig) -> Option<Check> {
    const NAME: &str = "models.audience";
    let signed: Vec<&str> = models
        .backends
        .iter()
        .filter(|(_, backend)| backend.auth.as_deref() == Some("turn-token"))
        .map(|(name, _)| name.as_str())
        .collect();
    if signed.is_empty() {
        return None;
    }
    let notes = models.audience_notes();
    Some(match notes.is_empty() {
        true => Check::ok(
            NAME,
            format!(
                "no signed token's audience names another host than its backend's url ({})",
                signed.join(", ")
            ),
        ),
        false => Check::warn(
            NAME,
            notes.join("; "),
            "make url the audience's AIPlex access point, or the audience the AIPlex the url \
             names (docs/model-gateway.md#a-token-branchyard-signs)",
        ),
    })
}

/// One backend and where its key is.
struct KeyRow {
    backend: String,
    api: String,
    /// `$VAR` or `@path`; `None` when the backend names no key.
    source: Option<String>,
    /// Whether that source holds a key here; `None` without a source.
    present: Option<bool>,
    /// Whether it is called with a token the server signs for each turn
    /// (`auth = "turn-token"`), which needs no key.
    signed: bool,
    /// Whether it is a control plane's access point (`control_plane`).
    plane: bool,
}

/// Whether `source` (`$VAR`) is a variable a provider's own key is read
/// from (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, ...): `by models list`
/// reads those for the providers' sources.
fn provider_variable(source: &str) -> bool {
    use branchyard::models::discover::{ANTHROPIC_KEYS, GEMINI_KEYS, OPENAI_KEYS, ZAI_KEYS};
    source.strip_prefix('$').is_some_and(|name| {
        [
            &GEMINI_KEYS[..],
            &OPENAI_KEYS[..],
            &ANTHROPIC_KEYS[..],
            &ZAI_KEYS[..],
        ]
        .concat()
        .contains(&name)
    })
}

fn key_present(source: &branchyard::models::KeySource, var_set: &dyn Fn(&str) -> bool) -> bool {
    match source {
        branchyard::models::KeySource::Env(var) => var_set(var),
        branchyard::models::KeySource::File(path) => path.is_file(),
    }
}

fn keys_row(rows: &[KeyRow]) -> Check {
    const NAME: &str = "models.keys";
    let mut parts = Vec::new();
    let mut status = Status::Ok;
    for row in rows {
        let (text, worst) = match (&row.source, row.present) {
            _ if row.signed => (
                format!(
                    "{} is called with a token the server signs for each turn (auth = turn-token)",
                    row.backend
                ),
                Status::Ok,
            ),
            // A control plane's token under a provider's variable name:
            // accepted, and said, since the name suggests another reader.
            (Some(source), Some(true)) if row.plane && provider_variable(source) => (
                format!(
                    "{} key {source} is set ({source} is a provider's variable name: its value \
                     is sent to this AIPlex backend and to nothing else, and it is hidden from \
                     every harness)",
                    row.backend
                ),
                Status::Ok,
            ),
            (Some(source), Some(true)) => {
                (format!("{} key {source} is set", row.backend), Status::Ok)
            }
            (Some(source), _) => (
                format!("{} key {source} is NOT set", row.backend),
                Status::Fail,
            ),
            // A local generic backend may take no key.
            (None, _) if row.api == "generic" => {
                (format!("{} takes no key", row.backend), Status::Ok)
            }
            (None, _) => (
                format!("{} ({}) names no key", row.backend, row.api),
                Status::Warn,
            ),
        };
        status = status.max(worst);
        parts.push(text);
    }
    let detail = parts.join("; ");
    match status {
        Status::Ok | Status::Todo => Check::ok(NAME, detail),
        Status::Warn => Check::warn(
            NAME,
            detail,
            "name the secret holding its key under key in [models.backends.NAME]",
        ),
        Status::Fail => Check::fail(
            NAME,
            detail,
            "export the variable (or create the file) the key names; the gateway reads it when a \
             turn starts",
        ),
    }
}

/// Every routed model has a price on every backend the gateway may send it
/// to, as the model that backend is asked for, from `[models.prices]` or the
/// catalog.
pub(super) fn models_prices(ctx: &Context) -> Vec<Check> {
    const NAME: &str = "models.prices";
    let Some(config) = ctx.config() else {
        return vec![skipped(
            NAME,
            "the configuration does not load",
            "config.merged",
        )];
    };
    if config.models.routes.is_empty() {
        return vec![Check::ok(
            NAME,
            match config.models.is_empty() {
                true => "not configured: no [models]",
                false => "no [[models.routes]], so no model is routed",
            },
        )];
    }
    let engine = match branchyard::configure::models_config(&config.models) {
        Ok(engine) => engine,
        Err(error) => {
            return vec![Check::fail(
                NAME,
                format!("[models] does not convert for the gateway: {error}"),
                "check [models] with `by config validate`",
            )]
        }
    };
    let mut unpriced: Vec<String> = Vec::new();
    let mut patterns = Vec::new();
    for route in &engine.routes {
        if route.model.contains(['*', '?']) {
            patterns.push(route.model.as_str());
            continue;
        }
        // Every backend the gateway may send the route's calls to, priced
        // as the model it asks that backend for: the route's, or its
        // backend_model where the call is translated. A route none of
        // whose backends is defined is priced as the request itself.
        let served = doctor::served_by(&engine, route);
        if served.is_empty() && doctor::price_source(&engine, Api::Generic, &route.model).is_none()
        {
            unpriced.push(route.model.clone());
        }
        for place in served {
            if doctor::price_source(&engine, place.api, &place.model).is_some() {
                continue;
            }
            let entry = match place.model == route.model {
                true => route.model.clone(),
                false => format!("{} via {} as {}", route.model, place.backend, place.model),
            };
            if !unpriced.contains(&entry) {
                unpriced.push(entry);
            }
        }
    }
    let mut detail = format!(
        "{} route{}",
        engine.routes.len(),
        if engine.routes.len() == 1 { "" } else { "s" }
    );
    if !patterns.is_empty() {
        detail.push_str(&format!(
            "; patterns ({}) are priced per model when called, not checked here",
            patterns.join(", ")
        ));
    }
    if unpriced.is_empty() {
        detail.push_str("; every named model has a price");
        return vec![Check::ok(NAME, detail)];
    }
    vec![Check::warn(
        NAME,
        format!(
            "{detail}; no price for {}: its calls are recorded unpriced, and a dollar budget \
             cannot count them",
            unpriced.join(", ")
        ),
        "add [models.prices.\"MODEL\"] with input and output dollars per million tokens",
    )]
}

// Small helpers.

/// The first line of `text`.
fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

/// The first twelve characters of a digest.
fn short(digest: &str) -> &str {
    digest.get(..12).unwrap_or(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `harnesses.google_cloud` for a machine with these variables, files
    /// and DMI product name.
    fn google_cloud(vars: &[(&str, &str)], files: &[&str], product: Option<&str>) -> Option<Check> {
        let vars: BTreeMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        let files: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
        GoogleCloud {
            var: &|name| vars.get(name).cloned(),
            exists: &|path| files.iter().any(|f| f == path),
            product: product.map(str::to_owned),
        }
        .check()
    }

    const GCE: Option<&str> = Some("Google Compute Engine");
    const ADC: &str = "/home/me/.config/gcloud/application_default_credentials.json";

    #[test]
    fn no_sign_of_google_cloud_is_no_row() {
        assert_eq!(google_cloud(&[("HOME", "/home/me")], &[], None), None);
        // A Google VM is where credentials may come from, not a reason to
        // report: with nothing pointed at Vertex AI there is no row.
        assert_eq!(google_cloud(&[("HOME", "/home/me")], &[], GCE), None);
    }

    #[test]
    fn adc_is_looked_for_in_the_order_google_libraries_look() {
        let key = google_cloud(
            &[
                ("HOME", "/home/me"),
                ("GOOGLE_APPLICATION_CREDENTIALS", "/keys/sa.json"),
            ],
            &["/keys/sa.json", ADC],
            GCE,
        )
        .unwrap();
        assert_eq!(key.status, Status::Ok);
        assert!(key.detail.starts_with(
            "Application Default Credentials: the file GOOGLE_APPLICATION_CREDENTIALS names"
        ));
        let missing = google_cloud(
            &[("GOOGLE_APPLICATION_CREDENTIALS", "/keys/gone.json")],
            &[ADC],
            GCE,
        )
        .unwrap();
        assert_eq!(missing.status, Status::Warn);
        assert!(missing
            .detail
            .contains("/keys/gone.json, which does not exist"));
        let gcloud = google_cloud(&[("HOME", "/home/me")], &[ADC], GCE).unwrap();
        assert_eq!(gcloud.status, Status::Ok);
        assert!(gcloud.detail.contains(
            "~/.config/gcloud/application_default_credentials.json, from `gcloud auth \
             application-default login` (isolated and sandboxed branches get a copy with \
             [secrets] GCLOUD_ADC = \"@~/.config/gcloud/application_default_credentials.json\")"
        ));
        let moved = google_cloud(
            &[("HOME", "/home/me"), ("CLOUDSDK_CONFIG", "/etc/gcloud")],
            &["/etc/gcloud/application_default_credentials.json"],
            None,
        )
        .unwrap();
        assert!(moved
            .detail
            .contains("/etc/gcloud/application_default_credentials.json"));
        let vm = google_cloud(
            &[("HOME", "/home/me"), ("GOOGLE_CLOUD_PROJECT", "p")],
            &[],
            GCE,
        )
        .unwrap();
        assert_eq!(vm.status, Status::Ok);
        assert_eq!(
            vm.detail,
            "Application Default Credentials: the metadata server's service account, on this \
             Google Compute Engine VM (its scopes are not checked: Vertex AI needs \
             cloud-platform); Vertex AI variables set: GOOGLE_CLOUD_PROJECT (names only)"
        );
        let file_alone = google_cloud(&[("HOME", "/home/me")], &[ADC], None).unwrap();
        assert!(file_alone
            .detail
            .ends_with("; no Vertex AI variable is set"));
    }

    #[test]
    fn vertex_ai_without_adc_is_a_todo_that_names_the_application_default_login() {
        let vars = [
            ("HOME", "/home/me"),
            ("CLAUDE_CODE_USE_VERTEX", "1"),
            ("ANTHROPIC_VERTEX_PROJECT_ID", "secret-project-name"),
        ];
        let row = google_cloud(&vars, &[], None).unwrap();
        assert_eq!(row.status, Status::Todo);
        assert_eq!(
            row.detail,
            "Vertex AI variables set: CLAUDE_CODE_USE_VERTEX, ANTHROPIC_VERTEX_PROJECT_ID \
             (names only), but no Application Default Credentials were found (no \
             GOOGLE_APPLICATION_CREDENTIALS, no \
             ~/.config/gcloud/application_default_credentials.json, and this is not a Google \
             Compute Engine VM)"
        );
        assert!(row
            .fix
            .unwrap()
            .starts_with("run `gcloud auth application-default login` (not `gcloud auth login`)"));
        // Values are never shown, only names.
        for row in [
            google_cloud(&vars, &[], None),
            google_cloud(&vars, &[ADC], GCE),
        ] {
            assert!(!row.unwrap().detail.contains("secret-project-name"));
        }
    }

    #[test]
    fn a_region_with_no_switch_is_not_vertex_ai_in_use() {
        let row = google_cloud(
            &[
                ("CLAUDE_CODE_USE_VERTEX", "0"),
                ("CLOUD_ML_REGION", "us-east5"),
            ],
            &[],
            None,
        )
        .unwrap();
        assert_eq!(row.status, Status::Ok);
        assert!(row.detail.contains("no harness is switched to Vertex AI"));
        // Gemini CLI's switch is exactly `true`; OpenCode's is a project.
        let gemini = google_cloud(&[("GOOGLE_GENAI_USE_VERTEXAI", "1")], &[], None).unwrap();
        assert_eq!(gemini.status, Status::Ok);
        let opencode = google_cloud(&[("GCLOUD_PROJECT", "p")], &[], None).unwrap();
        assert_eq!(opencode.status, Status::Todo);
        // Antigravity's ADC switch and Hermes's project are Vertex AI too.
        for vars in [[("AGY_ADC_AUTH", "true")], [("VERTEX_PROJECT_ID", "p")]] {
            let row = google_cloud(&vars, &[], None).unwrap();
            assert_eq!(row.status, Status::Todo, "{vars:?}");
        }
        let off = google_cloud(&[("AGY_ADC_AUTH", "false")], &[], None).unwrap();
        assert_eq!(off.status, Status::Ok);
    }
    use branchyard::services::ServiceOwner;
    use branchyard_testkit::{MockHttp, Response};
    use serde_json::json;
    use std::path::PathBuf;

    fn service(state: ServiceState, lease_until_ms: u64) -> Service {
        let mut service = Service::new("sandbox", ServiceOwner::this_process());
        service.state = state;
        service.lease_until_ms = lease_until_ms;
        service
    }

    #[test]
    fn a_service_is_stale_when_expired_reclaiming_or_live_past_its_lease() {
        let now = 1_000;
        let all = [
            service(ServiceState::Live, 2_000),
            service(ServiceState::Live, 900),
            service(ServiceState::Expired, 0),
            service(ServiceState::Reclaiming, 0),
            service(ServiceState::Left, 0),
            service(ServiceState::Reclaimed, 0),
        ];
        let stale: Vec<(ServiceState, u64)> = stale_services(&all, now)
            .iter()
            .map(|s| (s.state, s.lease_until_ms))
            .collect();
        assert_eq!(
            stale,
            [
                (ServiceState::Live, 900),
                (ServiceState::Expired, 0),
                (ServiceState::Reclaiming, 0)
            ]
        );
        assert!(stale_services(&all[..1], now).is_empty());
    }

    fn harness(id: &str, version: Option<&str>, logged_out: bool) -> HarnessState {
        let mut state = HarnessState {
            id: id.into(),
            path: format!("/usr/bin/{id}"),
            on_path: true,
            version: version.map(str::to_owned),
            ..HarnessState::default()
        };
        state.login.evidence = branchyard::inventory::Evidence::Likely;
        match logged_out {
            true => {
                state.login.state = LoginState::LoggedOut;
                state.login.detail = "no credentials".into();
            }
            false => state.login.state = LoginState::LoggedIn,
        }
        state
    }

    fn profile(harness: &str, available: bool, qualification: Option<&str>) -> HarnessInfo {
        HarnessInfo {
            harness: harness.into(),
            profile: harness.into(),
            default: true,
            available,
            qualification: qualification.map(str::to_owned),
        }
    }

    #[test]
    fn a_server_url_is_shown_without_its_credentials() {
        assert_eq!(
            without_userinfo("https://me:secret@by.example:8421/api"),
            "https://by.example:8421/api"
        );
        assert_eq!(
            without_userinfo("http://127.0.0.1:8421"),
            "http://127.0.0.1:8421"
        );
        assert_eq!(without_userinfo("ssh://me@host"), "ssh://host");
    }

    #[test]
    fn harnesses_are_a_summary_and_one_short_row_each() {
        let none = harness_rows(&[], &[]);
        assert_eq!(none.len(), 1);
        assert_eq!(none[0].status, Status::Warn);
        assert!(none[0]
            .fix
            .clone()
            .unwrap()
            .contains("by harnesses install"));

        let catalog = [
            profile("codex", true, Some("9/9 on codex-cli 0.5.1")),
            profile("gemini-cli", true, None),
        ];
        let installed = [
            harness("codex", Some("0.5.1"), false),
            harness("gemini-cli", None, true),
        ];
        let rows = harness_rows(&installed, &catalog);
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "harnesses.summary",
                "harnesses.codex",
                "harnesses.gemini-cli"
            ]
        );
        assert_eq!(rows[0].status, Status::Ok, "one can run: {rows:?}");
        assert_eq!(
            rows[0].detail,
            "2 installed: 1 logged in, 1 login not verified"
        );
        // The version, the login and the live run, matching versions.
        assert_eq!(rows[1].status, Status::Ok);
        assert_eq!(rows[1].detail, "0.5.1 · credentials found · live 9/9");
        // An unconfirmed login is a to-do that says so, never "cannot run".
        assert_eq!(rows[2].status, Status::Todo);
        assert_eq!(
            rows[2].detail,
            "version unknown · login not verified · no live run"
        );
        assert!(!rows[2].detail.contains("cannot run"));
        assert!(rows[2]
            .fix
            .clone()
            .unwrap()
            .contains("by harnesses login gemini-cli"));
        for row in &rows {
            assert!(row.detail.chars().count() <= 120, "{row:?}");
        }

        let rows = harness_rows(&installed[1..], &catalog);
        assert_eq!(rows[0].status, Status::Todo, "none can be used yet");
        assert!(rows[0].fix.clone().unwrap().contains("harnesses.*"));
    }

    #[test]
    fn a_live_run_on_another_version_is_named_beside_the_installed_one() {
        let catalog = [profile("codex", true, Some("9/9 on codex-cli 0.157.1"))];
        let installed = [harness("codex", Some("0.164.0"), false)];
        let row = harness_row(&installed[0], &catalog);
        assert_eq!(
            row.detail,
            "installed 0.164.0 · credentials found · live-tested 0.157.1 (9/9)"
        );
        // A harness whose version command was too slow says so.
        let mut slow = harness("codex", None, false);
        slow.version_note = Some("slow to start (took >10 s)".into());
        let row = harness_row(&slow, &catalog);
        assert!(row.detail.starts_with("slow to start · "), "{}", row.detail);
    }

    #[test]
    fn the_default_harness_is_checked_against_what_branchyard_knows_and_has() {
        let catalog = [
            profile("codex", true, None),
            profile("claude-code", false, None),
        ];
        let row = default_harness_row(Some("codex"), &catalog, None);
        assert_eq!((row.status, row.fix), (Status::Ok, None));
        let row = default_harness_row(Some("claude-code"), &catalog, None);
        assert_eq!(row.status, Status::Fail);
        assert!(row.detail.contains("is not installed"));
        assert!(row
            .fix
            .unwrap()
            .contains("by harnesses install claude-code"));
        let row = default_harness_row(Some("nope"), &catalog, None);
        assert_eq!(row.status, Status::Fail);
        assert!(row
            .detail
            .contains("not a harness or profile Branchyard knows"));
        // Unset: the built-in default, which must be installed.
        let row = default_harness_row(None, &catalog, None);
        assert_eq!(row.status, Status::Warn);
        assert!(row
            .detail
            .contains("built-in default claude-code is not installed"));
        let row = default_harness_row(None, &[profile("claude-code", true, None)], None);
        assert_eq!(row.status, Status::Ok);
    }

    fn held(name: &str, kind: HeldKind, used_ms: u64) -> HeldSandbox {
        HeldSandbox {
            branch: format!("b-{name}"),
            name: name.into(),
            kind,
            turn: None,
            used_ms,
        }
    }

    #[test]
    fn a_default_harness_whose_login_is_not_verified_is_a_warning_with_the_login_command() {
        let catalog = [profile("claude-code", true, None)];
        let out = harness("claude-code", Some("2.1.310"), true);
        let row = default_harness_row(None, &catalog, Some(std::slice::from_ref(&out)));
        assert_eq!(row.status, Status::Warn, "{row:?}");
        assert!(
            row.detail
                .contains("uses claude-code, which is installed, but its login is not verified"),
            "{}",
            row.detail
        );
        assert!(row.fix.unwrap().contains("by harnesses login claude-code"));
        // Logged in: as before.
        let ok = harness("claude-code", Some("2.1.310"), false);
        let row = default_harness_row(Some("claude-code"), &catalog, Some(&[ok]));
        assert_eq!(row.status, Status::Ok, "{row:?}");
    }

    #[test]
    fn kept_sandboxes_are_aged_against_max_kept_minutes() {
        let hour = 3_600_000;
        let now = 10 * hour;
        let rows = [
            held("fresh", HeldKind::Kept, now - 60_000),
            held("old", HeldKind::Kept, now - 3 * hour),
            held("snap", HeldKind::Snapshot, now - 5 * hour),
        ];
        // No limit: counted, the oldest named, and nothing to do.
        let row = sandboxes_row(&rows, None, now);
        assert_eq!(row.status, Status::Ok);
        assert!(
            row.detail
                .starts_with("2 kept sandboxes, 1 snapshot; the oldest unused for 3h"),
            "{}",
            row.detail
        );
        assert!(row.detail.contains("no max_kept_minutes"));
        // A limit the old one is over: warned, named with its branch.
        let row = sandboxes_row(&rows, Some(60), now);
        assert_eq!(row.status, Status::Warn);
        assert!(
            row.detail
                .contains("longer than max_kept_minutes = 60: old (b-old)"),
            "{}",
            row.detail
        );
        assert!(!row.detail.contains("fresh ("), "{}", row.detail);
        assert!(row.fix.unwrap().contains("by rm BRANCH"));
        // A limit nothing is over.
        let row = sandboxes_row(&rows, Some(600), now);
        assert_eq!(row.status, Status::Ok);
        assert!(row.detail.contains("none over max_kept_minutes = 600"));
        // Snapshots are never aged.
        let row = sandboxes_row(&rows[2..], Some(1), now);
        assert_eq!(row.status, Status::Ok);
        assert!(row.detail.starts_with("0 kept sandboxes, 1 snapshot"));
    }

    #[test]
    fn microsandbox_options_follow_the_configuration_section() {
        let section = Microsandbox {
            image: "alpine:3.20".into(),
            cpus: Some(2),
            memory_mib: Some(512),
            pass_env: vec!["A".into()],
            keep: Some("pause".into()),
            snapshots: Some(1),
            max_paused: Some(3),
            max_kept_minutes: Some(30),
            live_branch: Some(true),
        };
        let options = sandbox_options(&section);
        assert_eq!(options.image, "alpine:3.20");
        assert_eq!(options.keep, SandboxKeep::Pause);
        assert_eq!(options.max_kept_secs, Some(1_800));
        assert!(options.live_branch);
        let options = sandbox_options(&Microsandbox {
            keep: None,
            live_branch: None,
            max_kept_minutes: None,
            ..section
        });
        assert_eq!(options.keep, SandboxKeep::Destroy);
        assert_eq!(options.max_kept_secs, None);
        assert!(!options.live_branch);
    }

    #[test]
    fn a_secret_is_missing_when_its_variable_is_unset_or_its_file_absent() {
        let secrets: BTreeMap<String, String> = [
            ("SET".to_owned(), "HAS_VALUE".to_owned()),
            ("UNSET".to_owned(), "NO_VALUE".to_owned()),
            ("FILE".to_owned(), "@/nonexistent/doctor-secret".to_owned()),
        ]
        .into();
        let set = |name: &str| name == "HAS_VALUE";
        let missing = missing_secrets(Path::new("/repo"), &secrets, &set);
        assert_eq!(
            missing,
            [
                "FILE (from @/nonexistent/doctor-secret)",
                "UNSET (from $NO_VALUE)"
            ]
        );
    }

    #[test]
    fn model_keys_are_ok_only_when_every_named_key_is_set() {
        let row = |backend: &str, api: &str, source: Option<&str>, present: Option<bool>| KeyRow {
            backend: backend.into(),
            api: api.into(),
            source: source.map(str::to_owned),
            present,
            signed: false,
            plane: false,
        };
        let all_set = keys_row(&[
            row("a", "anthropic", Some("$A_KEY"), Some(true)),
            row("local", "generic", None, None),
        ]);
        assert_eq!(all_set.status, Status::Ok);
        assert_eq!(all_set.detail, "a key $A_KEY is set; local takes no key");
        let one_missing = keys_row(&[
            row("a", "anthropic", Some("$A_KEY"), Some(true)),
            row("o", "openai", Some("$O_KEY"), Some(false)),
        ]);
        assert_eq!(one_missing.status, Status::Fail);
        assert!(one_missing.detail.contains("o key $O_KEY is NOT set"));
        // An anthropic or openai backend with no key sends its requests bare.
        let keyless = keys_row(&[row("a", "anthropic", None, None)]);
        assert_eq!(keyless.status, Status::Warn);
        assert!(keyless.detail.contains("a (anthropic) names no key"));
        // A backend called with the server's signed token needs no key.
        let signed = keys_row(&[KeyRow {
            signed: true,
            ..row("aiplex", "anthropic", None, None)
        }]);
        assert_eq!(signed.status, Status::Ok);
        assert_eq!(
            signed.detail,
            "aiplex is called with a token the server signs for each turn (auth = turn-token)"
        );
        // A control plane's token read from a provider's variable name is
        // accepted, and the row says who is sent it; a plain backend's
        // provider variable, and a control plane's own variable, say nothing.
        let named = keys_row(&[
            KeyRow {
                plane: true,
                ..row("ap", "anthropic", Some("$ANTHROPIC_API_KEY"), Some(true))
            },
            KeyRow {
                plane: true,
                ..row("ap-own", "openai", Some("$AIPLEX_TOKEN"), Some(true))
            },
            row("plain", "openai", Some("$OPENAI_API_KEY"), Some(true)),
        ]);
        assert_eq!(named.status, Status::Ok);
        assert_eq!(
            named.detail,
            "ap key $ANTHROPIC_API_KEY is set ($ANTHROPIC_API_KEY is a provider's variable \
             name: its value is sent to this AIPlex backend and to nothing else, and it is \
             hidden from every harness); ap-own key $AIPLEX_TOKEN is set; plain key \
             $OPENAI_API_KEY is set"
        );
        for variable in [
            "$GEMINI_API_KEY",
            "$GOOGLE_API_KEY",
            "$OPENAI_API_KEY",
            "$CODEX_API_KEY",
            "$ANTHROPIC_API_KEY",
            "$ZAI_API_KEY",
        ] {
            assert!(provider_variable(variable), "{variable}");
        }
        for other in [
            "$AIPLEX_TOKEN",
            "ANTHROPIC_API_KEY",
            "@/run/ANTHROPIC_API_KEY",
            "",
        ] {
            assert!(!provider_variable(other), "{other}");
        }
    }

    #[test]
    fn the_url_row_is_one_row_and_only_for_control_plane_backends() {
        let models = |backends: serde_json::Value| -> branchyard_setup::config::ModelsConfig {
            serde_json::from_value(serde_json::json!({ "backends": backends })).unwrap()
        };
        let plane = |url: &str| {
            serde_json::json!({
                "api": "anthropic", "url": url, "control_plane": "aiplex", "key": "aiplex",
            })
        };
        // No control-plane backend: no row, whatever a plain backend's url.
        assert_eq!(transport_row(&models(serde_json::json!({}))), None);
        assert_eq!(
            transport_row(&models(serde_json::json!({
                "plain": {"api": "generic", "url": "http://10.0.0.5:7400"}
            }))),
            None
        );
        // https, or http on this machine: ok, naming the backends.
        let ok = transport_row(&models(serde_json::json!({
            "a": plane("https://aiplex.example.com/ap/ide"),
            "b": plane("http://127.0.0.1:7400/ap/ide"),
            "c": plane("http://localhost:7400/ap/ide"),
        })))
        .unwrap();
        assert_eq!((ok.name.as_str(), ok.status), ("models.url", Status::Ok));
        assert_eq!(
            ok.detail,
            "every control-plane backend's url is https, or http on this machine (a, b, c)"
        );
        assert_eq!(ok.fix, None);
        // Plain http anywhere else: one warning for all of them.
        let warn = transport_row(&models(serde_json::json!({
            "a": plane("http://aiplex.internal:8080/ap/ide"),
            "b": plane("https://aiplex.example.com/ap/ide"),
            "c": plane("http://10.0.0.5/ap/ide"),
        })))
        .unwrap();
        assert_eq!(
            (warn.name.as_str(), warn.status),
            ("models.url", Status::Warn)
        );
        assert_eq!(
            warn.detail,
            "models.backends.a: its url is plain http on aiplex.internal, which is not loopback, \
             so the credential it is sent travels unencrypted unless the network between \
             Branchyard and AIPlex encrypts it; use https:// if it does not; models.backends.c: \
             its url is plain http on 10.0.0.5, which is not loopback, so the credential it is \
             sent travels unencrypted unless the network between Branchyard and AIPlex encrypts \
             it; use https:// if it does not"
        );
        assert_eq!(
            warn.fix.as_deref(),
            Some(
                "use an https:// url, or make sure the network between Branchyard and AIPlex \
                 encrypts what it carries (docs/model-gateway.md#configuring-it)"
            )
        );
    }

    #[test]
    fn the_audience_row_is_one_row_and_only_for_backends_called_with_a_signed_token() {
        let models = |backends: serde_json::Value| -> branchyard_setup::config::ModelsConfig {
            serde_json::from_value(serde_json::json!({ "backends": backends })).unwrap()
        };
        let signed = |url: &str, audience: &str| {
            serde_json::json!({
                "api": "anthropic", "url": url, "control_plane": "aiplex",
                "auth": "turn-token", "audience": audience,
            })
        };
        // No backend is called with a signed token: no row.
        let stored = serde_json::json!({
            "api": "anthropic", "url": "https://aiplex.example.com/ap/ide",
            "control_plane": "aiplex", "key": "aiplex",
        });
        assert_eq!(audience_row(&models(serde_json::json!({}))), None);
        assert_eq!(
            audience_row(&models(serde_json::json!({ "stored": stored }))),
            None
        );
        // The audience on the url's host, in any letter case and with any
        // port, or one that is not a URL at all: ok, naming the backends.
        let ok = audience_row(&models(serde_json::json!({
            "a": signed("https://aiplex.example.com/ap/ide", "https://AIPLEX.example.com:8443"),
            "b": signed("https://aiplex.example.com/ap/ide", "aiplex-production"),
            "stored": stored,
        })))
        .unwrap();
        assert_eq!(ok.name, "models.audience");
        assert_eq!(ok.status, Status::Ok);
        assert_eq!(
            ok.detail,
            "no signed token's audience names another host than its backend's url (a, b)"
        );
        assert_eq!(ok.fix, None);
        // Another host: one warning for all of them, with what to do.
        let warn = audience_row(&models(serde_json::json!({
            "a": signed("https://gateway.example.net/ap/ide", "https://aiplex.example.com"),
            "b": signed("https://aiplex.example.com/ap/ide", "https://aiplex.example.com"),
            "c": signed("https://other.example.net/ap/ide", "https://aiplex.example.com"),
        })))
        .unwrap();
        assert_eq!(warn.name, "models.audience");
        assert_eq!(warn.status, Status::Warn);
        assert_eq!(
            warn.detail,
            "models.backends.a: its audience names the host aiplex.example.com and its url the \
             host gateway.example.net; the token it is sent is good at the audience for up to an \
             hour, so check the url is the audience's AIPlex; models.backends.c: its audience \
             names the host aiplex.example.com and its url the host other.example.net; the token \
             it is sent is good at the audience for up to an hour, so check the url is the \
             audience's AIPlex"
        );
        assert_eq!(
            warn.fix.as_deref(),
            Some(
                "make url the audience's AIPlex access point, or the audience the AIPlex the url \
                 names (docs/model-gateway.md#a-token-branchyard-signs)"
            )
        );
    }

    #[test]
    fn a_trust_state_is_a_row_with_what_to_do() {
        let resolved = |trust| workspace_cmd::Resolved {
            root: PathBuf::from("/repo"),
            workspace: branchyard_setup::config::WorkspaceConfig::default(),
            origin: branchyard_setup::config::WorkspaceOrigin::Project,
            file: PathBuf::from("/repo/branchyard.toml"),
            digest: "0123456789abcdef0123".into(),
            trust,
        };
        let row = trust_row(&resolved(TrustState::Trusted));
        assert_eq!(row.status, Status::Ok);
        assert!(row.detail.contains("digest 0123456789ab matches"));
        assert_eq!(
            trust_row(&resolved(TrustState::NotNeeded)).status,
            Status::Ok
        );
        let row = trust_row(&resolved(TrustState::Untrusted));
        assert_eq!(row.status, Status::Warn);
        assert!(row.detail.contains("scripts you have not trusted"));
        assert!(row.fix.unwrap().contains("by workspace trust"));
        let row = trust_row(&resolved(TrustState::Changed));
        assert_eq!(row.status, Status::Warn);
        assert!(row.detail.contains("changed since you trusted it"));
    }

    #[test]
    fn the_repositories_a_server_serves_are_listed_in_words() {
        assert_eq!(list(&[]), "no repositories");
        assert_eq!(list(&["a".into()]), "repository a");
        assert_eq!(list(&["a".into(), "b".into()]), "repositories a, b");
        assert_eq!(short("0123456789abcdef"), "0123456789ab");
        assert_eq!(short("abc"), "abc");
        assert_eq!(first_line("one\ntwo"), "one");
    }

    #[test]
    fn a_server_that_answers_is_read_and_one_that_hangs_ends_at_the_deadline() {
        let served = MockHttp::start(|_| {
            Response::json(200, &json!({"repos": [{"name": "app", "root": "/r"}]}))
        });
        let client = Client::new(&served.url(), "tok").unwrap();
        let names = within(Duration::from_secs(10), move || {
            client
                .repos()
                .map(|repos| repos.into_iter().map(|r| r.name).collect::<Vec<_>>())
        });
        assert_eq!(names.ok(), Some(vec!["app".to_owned()]));
        served.finish();

        let silent = MockHttp::start(|_| Response::hang());
        let client = Client::new(&silent.url(), "tok").unwrap();
        let began = std::time::Instant::now();
        let answer = within(Duration::from_millis(300), move || client.repos());
        assert!(matches!(answer, Err(None)), "{answer:?}");
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "{:?}",
            began.elapsed()
        );
        silent.finish();
    }

    fn problem(what: &str, error: &str, next: &str) -> branchyard_client::api::Problem {
        branchyard_client::api::Problem {
            kind: branchyard_client::api::ProblemKind::Trigger,
            key: what.into(),
            what: what.into(),
            since_ms: 1_790_000_000_000,
            failures: Some(3),
            error: error.into(),
            next: next.into(),
        }
    }

    const SHOWN: &str = "https://server.example";

    fn answered() -> Answer {
        Answer::Repositories {
            served: vec!["app".into()],
            wanted: None,
        }
    }

    #[test]
    fn no_problems_is_ok_and_a_listed_problem_names_the_oldest_and_what_to_do() {
        let row = problems_row(SHOWN, &answered(), || Ok(ProblemList::default()));
        assert_eq!(row, Some(Check::ok("server.problems", "no problems")));

        let paused = problem(
            "trigger nightly-triage",
            "paused after 3 failed runs in a row;\nthe last: branch_exists",
            "see why with `by trigger runs nightly-triage`, fix it, then `by trigger enable nightly-triage`",
        );
        let waiting = problem(
            "operation op_1",
            "no live worker carries gpu",
            "start a worker with --label gpu",
        );
        let listed = ProblemList {
            problems: vec![paused.clone(), waiting],
        };
        let row = problems_row(SHOWN, &answered(), || Ok(listed)).unwrap();
        assert_eq!(row.status, Status::Warn);
        assert_eq!(
            row.detail,
            "2 problems; the oldest: trigger nightly-triage: paused after 3 failed runs in a row; \
             the last: branch_exists"
        );
        assert_eq!(row.fix.as_deref(), Some(paused.next.as_str()));

        let one = ProblemList {
            problems: vec![paused],
        };
        let row = problems_row(SHOWN, &answered(), || Ok(one)).unwrap();
        assert!(
            row.detail
                .starts_with("1 problem: trigger nightly-triage: "),
            "{row:?}"
        );
    }

    #[test]
    fn a_server_without_the_route_is_to_be_upgraded_and_one_that_fails_says_how() {
        let missing = branchyard_client::Error::Api {
            status: 404,
            error: Box::new(branchyard_client::api::ErrorBody {
                code: "not_found".into(),
                message: "no route GET /v1/problems".into(),
                detail: None,
            }),
        };
        let row = problems_row(SHOWN, &answered(), || Err(Some(missing))).unwrap();
        assert_eq!(row.status, Status::Warn);
        assert!(row.detail.contains("has no GET /v1/problems"), "{row:?}");
        assert!(
            row.fix
                .as_deref()
                .unwrap()
                .starts_with("upgrade the server"),
            "{row:?}"
        );

        let row = problems_row(SHOWN, &answered(), || Err(None)).unwrap();
        assert_eq!(row.status, Status::Warn);
        assert!(row.detail.ends_with("no answer within 5s"), "{row:?}");
        let failed = branchyard_client::Error::Protocol("not JSON".into());
        let row = problems_row(SHOWN, &answered(), || Err(Some(failed))).unwrap();
        assert!(row.detail.contains("not JSON"), "{row:?}");
        assert!(row.fix.is_some());
    }

    #[test]
    fn problems_are_not_asked_of_a_server_whose_repository_list_was_not_read() {
        for answer in [
            Answer::Unreachable("connection refused".into()),
            Answer::Refused("the token has expired".into()),
            Answer::Unusable("no token file".into()),
            Answer::Failed("500".into()),
        ] {
            let row = problems_row(SHOWN, &answer, || panic!("asked after {answer:?}")).unwrap();
            assert_eq!(row.status, Status::Warn, "{answer:?}");
            assert!(row.detail.starts_with(NOT_CHECKED), "{row:?}");
            assert_eq!(row.fix.as_deref(), Some("fix server.reachable first"));
        }
        let ssh = Answer::NotAsked("doctor does not contact an ssh remote".into());
        assert_eq!(
            problems_row(SHOWN, &ssh, || panic!("asked an ssh remote")),
            None
        );
    }
}
