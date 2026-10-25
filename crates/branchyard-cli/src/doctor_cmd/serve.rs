//! The server you run from this repository: the file `[serve] config`
//! names. `server.reachable` (in `checks`) asks a server somewhere else
//! whether it answers; these two read the configuration of the one `by
//! serve` would start here, so a mistake in it shows before the first start.
//!
//! Neither row starts a server, opens a store or connects to a database.

use std::path::{Path, PathBuf};

use super::context::Context;
use super::{Check, NOT_CHECKED};

/// `[serve] config`, resolved against the repository, when it is set.
fn file(ctx: &Context) -> Option<PathBuf> {
    let configured = ctx.config()?.serve.config.as_deref()?;
    let path = Path::new(configured);
    Some(match path.is_absolute() {
        true => path.to_path_buf(),
        false => ctx.root().unwrap_or(&ctx.base).join(path),
    })
}

/// The arguments that make the server's own parser read `path`.
fn arguments(path: &Path) -> Vec<String> {
    vec!["--config".to_owned(), path.display().to_string()]
}

/// Whether the server configuration `[serve] config` names validates, as
/// `by serve --check` reads it. No row without `[serve] config`.
pub(super) fn config(ctx: &Context) -> Vec<Check> {
    const NAME: &str = "serve.config";
    let Some(path) = file(ctx) else {
        return Vec::new();
    };
    let (notes, warnings): (Vec<String>, Vec<String>) =
        match branchyard_server::cli::check(&arguments(&path)) {
            Ok(found) => found
                .into_iter()
                .partition(|w| w.starts_with(branchyard_server::cli::WOULD_CREATE_TOKEN)),
            Err(why) => {
                return vec![Check::fail(
                    NAME,
                    format!("{}: {why}", path.display()),
                    "fix the file, then run `by serve --check` (docs/server.md)",
                )]
            }
        };
    vec![match warnings.is_empty() {
        true => Check::ok(
            NAME,
            format!(
                "{} validates as `by serve --check` reads it{}",
                path.display(),
                // Routine before a first start: the token is made then.
                notes
                    .first()
                    .map_or_else(String::new, |note| format!("; {note}"))
            ),
        ),
        false => Check::warn(
            NAME,
            format!(
                "{} validates, with {} warning{}: {}",
                path.display(),
                warnings.len(),
                if warnings.len() == 1 { "" } else { "s" },
                warnings.join("; ")
            ),
            "run `by serve --check` to see them again, and fix what they name",
        ),
    }]
}

/// Which store the server configuration names, and whether this build can
/// open it. A name, not a connection: nothing is contacted.
pub(super) fn database(ctx: &Context) -> Vec<Check> {
    const NAME: &str = "serve.database";
    let Some(path) = file(ctx) else {
        return Vec::new();
    };
    let config = match branchyard_server::cli::resolve(&arguments(&path)) {
        Ok(config) => config,
        Err(_) => {
            return vec![Check::warn(
                NAME,
                format!("{NOT_CHECKED}the server configuration does not load"),
                "fix serve.config first",
            )]
        }
    };
    vec![match &config.database {
        None => Check::ok(
            NAME,
            format!(
                "SQLite in {}: one server; PostgreSQL would let several share the queue \
                 (docs/modes.md#postgres)",
                config.data_dir.join("state.db").display()
            ),
        ),
        Some(url) if !cfg!(feature = "postgres") => Check::fail(
            NAME,
            format!(
                "PostgreSQL at {}, but this build has no PostgreSQL support, so `by serve` \
                 refuses to start",
                place(url)
            ),
            format!(
                "rebuild with `{}`, or serve from a prebuilt release, which has it",
                branchyard_setup::probe::REBUILD_POSTGRES
            ),
        ),
        Some(url) => Check::ok(
            NAME,
            format!(
                "PostgreSQL at {} (named, not connected to here); `by serve` connects when it starts",
                place(url)
            ),
        ),
    }]
}

/// A PostgreSQL URL as host and database, without a user or password.
fn place(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let rest = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
    rest.split(['?', '#']).next().unwrap_or(rest).to_owned()
}

#[cfg(test)]
mod tests {
    use super::place;

    #[test]
    fn a_database_is_named_without_its_user_or_password() {
        assert_eq!(
            place("postgres://by:secret@db.example:5432/branchyard"),
            "db.example:5432/branchyard"
        );
        assert_eq!(
            place("postgresql://localhost/by?sslmode=require"),
            "localhost/by"
        );
        assert_eq!(
            place("postgres://branchyard@localhost/branchyard"),
            "localhost/branchyard"
        );
    }
}
