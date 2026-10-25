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
use branchyard_client::Client;
use branchyard_setup::config::{
    Microsandbox, NetworkEnforceMode, ProjectConfig, ProviderKind as ConfiguredProvider,
};
use branchyard_setup::probe::{Probe as _, KNOWN_VARIABLES};
use branchyard_support::time::now_ms;
use branchyard_workspace::Repository;

use super::context::{Context, Layer, Merged, Store};
use super::{Check, Status, NOT_CHECKED};
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
fn without_userinfo(url: &str) -> String {
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
            let before = workspace_cmd::trusted_digest(&resolved.root.display().to_string())
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
        Standing::Ready => Check::ok(name, detail),
        // A login not made yet is a step to take, not a fault.
        Standing::NotVerified(_) => Check::todo(name, detail, inventory::login_hint(&state.id)),
        Standing::CannotRun(_) if state.login.state == LoginState::LoggedOut => {
            Check::todo(name, detail, inventory::login_hint(&state.id))
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
        .map(|state| (state.standing(), inventory::login_hint(&state.id)));
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
    if missing.is_empty() {
        return vec![Check::ok(NAME, detail)];
    }
    detail.push_str(&format!(
        "; [secrets] without a source here: {}",
        missing.join(", ")
    ));
    vec![Check::warn(
        NAME,
        detail,
        "export the variable (or create the file) each secret names; isolated and sandboxed \
         branches are given them",
    )]
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
        ConfiguredProvider::Recipe => recipe_row(ctx, config),
    }]
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
            Ok(()) => Check::ok(
                NAME,
                format!(
                    "microsandbox with image {}: this build can run it",
                    options.image
                ),
            ),
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
    vec![match (platform.os.as_str(), platform.kvm) {
        ("linux", true) => Check::ok(NAME, "/dev/kvm is open to you: microVMs can start"),
        ("linux", false) => Check::warn(
            NAME,
            "no read-write access to /dev/kvm: `--provider microsandbox` cannot start a microVM here",
            "load the kvm module and add your user to the kvm group; local branches need none",
        ),
        (os, _) => Check::ok(NAME, format!("{os}: /dev/kvm does not apply")),
    }]
}

// Server.

/// A configured `[remote]` answers `GET /v1/repos` with its token.
pub(super) fn server_reachable(ctx: &Context) -> Vec<Check> {
    const NAME: &str = "server.reachable";
    let globals = &ctx.globals;
    let Some(url) = globals.remote.as_deref() else {
        return vec![Check::ok(
            NAME,
            "not configured: no --remote, BRANCHYARD_REMOTE or [remote] url; commands run here",
        )];
    };
    // A URL may carry `user:password@`: shown without it.
    let shown = without_userinfo(url);
    if crate::ssh_remote::is_ssh(url) {
        return vec![Check::ok(
            NAME,
            format!("{shown} is an ssh remote: not checked here; `by remote ssh status` shows it"),
        )];
    }
    let Some(token_file) = globals.token_file.as_deref() else {
        return vec![Check::fail(
            NAME,
            format!("{shown} is configured but no token file is"),
            "set token_file under [remote], or pass --token-file / BRANCHYARD_TOKEN_FILE",
        )];
    };
    let client = Client::from_token_file(url, token_file)
        .and_then(|client| match globals.ca_file.as_deref() {
            Some(ca) => client.with_ca_file(ca),
            None => Ok(client),
        })
        .map(|client| client.with_timeout(SERVER_DEADLINE));
    let client = match client {
        Ok(client) => client,
        Err(error) => {
            return vec![Check::fail(
                NAME,
                error.to_string().replace(url, &shown),
                "check the token file and CA file the remote names",
            )]
        }
    };
    vec![match served_repositories(client, SERVER_DEADLINE) {
        Err(why) => Check::fail(
            NAME,
            format!(
                "{shown} did not answer GET /v1/repos: {}",
                why.replace(url, &shown)
            ),
            crate::remote::UNREACHABLE_FIX,
        ),
        Ok(names) => match globals.repo.as_deref() {
            Some(wanted) if !names.iter().any(|n| n == wanted) => Check::fail(
                NAME,
                format!(
                    "{shown} answers, but serves {}, not {wanted:?}",
                    list(&names)
                ),
                "fix repo under [remote] (or --repo / BRANCHYARD_REPO), or serve it",
            ),
            _ => Check::ok(NAME, format!("{shown} answers; it serves {}", list(&names))),
        },
    }]
}

fn list(names: &[String]) -> String {
    match names {
        [] => "no repositories".to_owned(),
        [one] => format!("repository {one}"),
        several => format!("repositories {}", several.join(", ")),
    }
}

/// The names of the repositories the server serves, or why it did not say,
/// within `deadline` whatever the network does: the request runs on a
/// thread of its own, and one that outlives the deadline is left to end
/// with the process.
fn served_repositories(client: Client, deadline: Duration) -> Result<Vec<String>, String> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let answer = client
            .repos()
            .map(|repos| repos.into_iter().map(|r| r.name).collect::<Vec<_>>())
            .map_err(|error| error.to_string());
        branchyard_support::best_effort(
            "tell doctor what the server answered",
            sender.send(answer),
        );
    });
    receiver
        .recv_timeout(deadline)
        .map_err(|_| format!("no answer within {deadline:?}"))?
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
            }
        })
        .collect();
    vec![keys_row(&rows)]
}

/// One backend and where its key is.
struct KeyRow {
    backend: String,
    api: String,
    /// `$VAR` or `@path`; `None` when the backend names no key.
    source: Option<String>,
    /// Whether that source holds a key here; `None` without a source.
    present: Option<bool>,
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
        assert_eq!(
            served_repositories(client, Duration::from_secs(10)),
            Ok(vec!["app".to_owned()])
        );
        served.finish();

        let silent = MockHttp::start(|_| Response::hang());
        let client = Client::new(&silent.url(), "tok").unwrap();
        let began = std::time::Instant::now();
        let answer = served_repositories(client, Duration::from_millis(300));
        assert_eq!(answer, Err("no answer within 300ms".to_owned()));
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "{:?}",
            began.elapsed()
        );
        silent.finish();
    }
}
