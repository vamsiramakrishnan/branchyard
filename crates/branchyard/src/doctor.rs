//! What `by doctor` asks of the engine: facts about a repository's store,
//! its sandbox provider and this host, read without changing anything the
//! user keeps. Nothing here repairs; each function answers one question the
//! engine already knows the answer to, so the report and the engine cannot
//! disagree.
//!
//! - [`store_schema`] reads the schema version recorded in
//!   `.branchyard/state.db` through a read-only connection, before (and
//!   without) [`crate::Yard::open`], which upgrades an older store.
//! - [`check_microsandbox`] is the check `by run --provider microsandbox`
//!   makes before it creates anything.
//! - [`held_sandboxes`] lists the kept sandboxes and snapshots the store
//!   holds for a Microsandbox provider.
//! - [`egress_confinement`] says whether this host can confine a local
//!   harness to the egress proxy (docs/egress.md).
//! - [`price_source`] says what prices a routed model, as the model gateway
//!   does (docs/model-gateway.md).

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::models::{pricing, Api, Config, Tokens};
use crate::providers::ProviderKind;
use crate::state::SandboxKind;
use crate::{Error, SandboxOptions, Yard};

/// How long to wait for a write lock held by another process while reading
/// the schema.
const READ_WAIT: Duration = Duration::from_millis(500);

/// Where a store's recorded schema stands against the one this build reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchemaState {
    /// The version this build reads and writes.
    Current,
    /// An older version this build upgrades, once and in place, the first
    /// time it opens the store.
    Upgradable,
    /// A newer version: a later `by` wrote it, and this build refuses to
    /// open it.
    Newer,
    /// A version this build neither reads nor upgrades.
    Unsupported,
    /// The file holds no version yet: it was created and never opened by a
    /// build. The first open records one.
    Unrecorded,
}

/// The schema version recorded in a repository's `state.db`, and the one
/// this build supports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreSchema {
    /// The database file.
    pub path: PathBuf,
    /// The version recorded in it; `None` when it records none.
    pub found: Option<i64>,
    /// The version this build reads and writes.
    pub supported: i64,
}

impl StoreSchema {
    /// Where [`StoreSchema::found`] stands against
    /// [`StoreSchema::supported`].
    pub fn state(&self) -> SchemaState {
        match self.found {
            None => SchemaState::Unrecorded,
            Some(found) if found == self.supported => SchemaState::Current,
            // Version 1 is the one older layout `Sqlite::open` upgrades
            // (its `upgrade_to_identities` arm); a new migration there is a
            // new arm here, and the test below fails until it has one.
            Some(1) if self.supported == 2 => SchemaState::Upgradable,
            Some(found) if found > self.supported => SchemaState::Newer,
            Some(_) => SchemaState::Unsupported,
        }
    }
}

/// The schema version recorded in `root`'s `.branchyard/state.db`, read
/// through a read-only connection so that nothing is created, upgraded or
/// recovered. `Ok(None)` when the repository has no `state.db`.
///
/// # Errors
///
/// [`Error::State`] when the file exists but is not a readable SQLite
/// database.
pub fn store_schema(root: &Path) -> Result<Option<StoreSchema>, Error> {
    let path = crate::state::dir(root).join("state.db");
    if !path.is_file() {
        return Ok(None);
    }
    let unreadable = |error: rusqlite::Error| {
        Error::State(format!(
            "{} is not a readable state database: {error}",
            path.display()
        ))
    };
    let conn =
        Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(unreadable)?;
    conn.busy_timeout(READ_WAIT).map_err(unreadable)?;
    let recorded: Result<Option<String>, rusqlite::Error> = conn
        .query_row("SELECT value FROM meta WHERE key = 'schema'", [], |r| {
            r.get(0)
        })
        .optional();
    let found = match recorded {
        Ok(text) => text,
        // A database made and never opened by a build has no `meta` table.
        Err(rusqlite::Error::SqliteFailure(_, Some(message)))
            if message.contains("no such table") =>
        {
            None
        }
        Err(error) => return Err(unreadable(error)),
    };
    let found = match found {
        None => None,
        Some(text) => Some(text.trim().parse::<i64>().map_err(|_| {
            Error::State(format!(
                "{} records the schema version {text:?}, which is not a number",
                path.display()
            ))
        })?),
    };
    Ok(Some(StoreSchema {
        path,
        found,
        supported: SUPPORTED_SCHEMA,
    }))
}

/// The schema version this build reads and writes.
pub const SUPPORTED_SCHEMA: i64 = crate::sqlite::SCHEMA;

/// Refuse Microsandbox options this build cannot run, as the engine does
/// before it creates a branch: a build without the `microsandbox` feature,
/// or options with no image.
///
/// # Errors
///
/// [`Error::Unsupported`], saying what is missing.
pub fn check_microsandbox(yard: &Yard, options: &SandboxOptions) -> Result<(), Error> {
    ProviderKind::check(options, yard)
}

/// What a sandbox row holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeldKind {
    /// A sandbox kept (paused) between turns.
    Kept,
    /// A provider snapshot taken with a checkpoint.
    Snapshot,
}

/// A kept sandbox or snapshot the store holds for a provider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldSandbox {
    /// The branch that owns it.
    pub branch: String,
    /// The sandbox's name, or the snapshot's handle.
    pub name: String,
    /// Which it is.
    pub kind: HeldKind,
    /// For a snapshot, its checkpoint; for a kept sandbox, the checkpoint
    /// its state corresponds to, once recorded.
    pub turn: Option<u32>,
    /// When it was last parked or taken, in milliseconds since the Unix
    /// epoch. A kept sandbox older than the provider's `max_kept_secs` is
    /// destroyed instead of resumed.
    pub used_ms: u64,
}

/// The kept sandboxes and snapshots the store holds for the Microsandbox
/// provider, least recently used first.
///
/// # Errors
///
/// [`Error::State`] when the store cannot be read.
pub fn held_sandboxes(yard: &Yard, options: &SandboxOptions) -> Result<Vec<HeldSandbox>, Error> {
    let key = ProviderKind::key(options);
    let store = yard.store();
    let mut held = Vec::new();
    for kind in [SandboxKind::Kept, SandboxKind::Snapshot] {
        for row in store.sandboxes().sandboxes_of(kind, &key)? {
            held.push(HeldSandbox {
                branch: row.branch,
                name: row.name,
                kind: match kind {
                    SandboxKind::Kept => HeldKind::Kept,
                    SandboxKind::Snapshot => HeldKind::Snapshot,
                },
                turn: row.turn,
                used_ms: row.used_ms,
            });
        }
    }
    held.sort_by(|a, b| a.used_ms.cmp(&b.used_ms).then(a.name.cmp(&b.name)));
    Ok(held)
}

/// Whether this host can confine a local harness to the egress proxy in its
/// own user and network namespace, and why not (docs/egress.md). The
/// answer is the one a turn with an enforced network policy gets: it is
/// detected by confining `/bin/sh -c 'exit 0'`, once per process, and
/// `BRANCHYARD_EGRESS_NETNS=off` turns it off.
///
/// # Errors
///
/// The reason confinement is unavailable, as the engine records it.
pub fn egress_confinement() -> Result<(), String> {
    branchyard_runtime::LocalProvider::confinement()
}

/// What prices a model on a backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PriceSource {
    /// An entry of `[models.prices]`, by the pattern that matched.
    Configured(String),
    /// The built-in pricing catalog.
    Catalog,
}

/// What prices `model` called on `api`: a `[models.prices]` pattern first,
/// as the gateway looks, then the catalog; `None` when neither does, so the
/// gateway would record its calls unpriced and a dollar budget could not
/// count them.
pub fn price_source(config: &Config, api: Api, model: &str) -> Option<PriceSource> {
    let configured = config
        .prices
        .keys()
        .filter(|pattern| branchyard_provision::connectors::glob_match(pattern, model))
        .max_by_key(|pattern| pattern.len());
    if let Some(pattern) = configured {
        return Some(PriceSource::Configured(pattern.clone()));
    }
    pricing::cost(api, model, &Tokens::default()).map(|_| PriceSource::Catalog)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Price;
    use crate::state::SandboxRow;
    use crate::SandboxOptions;

    fn repository() -> tempfile::TempDir {
        let dir = tempfile::Builder::new()
            .prefix("by-doctor-")
            .tempdir()
            .unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        dir
    }

    fn set_schema(root: &Path, version: &str) {
        let conn = Connection::open(root.join(".branchyard/state.db")).unwrap();
        conn.execute("UPDATE meta SET value = ?1 WHERE key = 'schema'", [version])
            .unwrap();
    }

    #[test]
    fn no_state_database_is_no_schema_and_nothing_is_created() {
        let dir = repository();
        assert_eq!(store_schema(dir.path()).unwrap(), None);
        assert!(!dir.path().join(".branchyard").exists());
    }

    #[test]
    fn the_schema_is_read_without_upgrading_it() {
        let dir = repository();
        drop(Yard::open(dir.path()).unwrap());
        let schema = store_schema(dir.path()).unwrap().unwrap();
        assert_eq!(schema.found, Some(schema.supported));
        assert_eq!(schema.state(), SchemaState::Current);
        assert!(schema.path.ends_with(".branchyard/state.db"));

        // An older store is reported as one the next open upgrades, and
        // reading it leaves it as it was.
        set_schema(dir.path(), "1");
        let schema = store_schema(dir.path()).unwrap().unwrap();
        assert_eq!(schema.found, Some(1));
        assert_eq!(schema.state(), SchemaState::Upgradable);
        assert_eq!(store_schema(dir.path()).unwrap().unwrap().found, Some(1));

        set_schema(dir.path(), "99");
        assert_eq!(
            store_schema(dir.path()).unwrap().unwrap().state(),
            SchemaState::Newer
        );
        set_schema(dir.path(), "0");
        assert_eq!(
            store_schema(dir.path()).unwrap().unwrap().state(),
            SchemaState::Unsupported
        );
        set_schema(dir.path(), "two");
        let error = store_schema(dir.path()).unwrap_err().to_string();
        assert!(error.contains("not a number"), "{error}");
    }

    #[test]
    fn a_file_that_is_not_a_database_is_an_error_naming_it() {
        let dir = repository();
        std::fs::create_dir_all(dir.path().join(".branchyard")).unwrap();
        std::fs::write(
            dir.path().join(".branchyard/state.db"),
            "this is not a database, and is longer than a header",
        )
        .unwrap();
        let error = store_schema(dir.path()).unwrap_err().to_string();
        assert!(error.contains("state.db"), "{error}");
        assert!(error.contains("not a readable state database"), "{error}");
    }

    #[test]
    fn a_database_with_no_meta_table_records_no_version() {
        let dir = repository();
        std::fs::create_dir_all(dir.path().join(".branchyard")).unwrap();
        Connection::open(dir.path().join(".branchyard/state.db"))
            .unwrap()
            .execute_batch("CREATE TABLE unrelated (a)")
            .unwrap();
        let schema = store_schema(dir.path()).unwrap().unwrap();
        assert_eq!(schema.found, None);
        assert_eq!(schema.state(), SchemaState::Unrecorded);
    }

    #[test]
    fn the_microsandbox_check_is_the_engines() {
        let dir = repository();
        let yard = Yard::open(dir.path()).unwrap();
        let error = check_microsandbox(&yard, &SandboxOptions::default())
            .unwrap_err()
            .to_string();
        // Without the feature the build is refused first; with it, the image.
        assert!(
            error.contains("Microsandbox support") || error.contains("needs an image"),
            "{error}"
        );
        let named = SandboxOptions {
            image: "alpine:3.20".into(),
            ..SandboxOptions::default()
        };
        // With the feature, an image is enough; without it, the build is
        // what is refused.
        match check_microsandbox(&yard, &named) {
            Ok(()) => {}
            Err(error) => assert!(error.to_string().contains("Microsandbox support")),
        }
    }

    #[test]
    fn held_sandboxes_are_those_of_the_providers_key_oldest_first() {
        let dir = repository();
        let yard = Yard::open(dir.path()).unwrap();
        let row = |name: &str, kind, provider: &str, used_ms| SandboxRow {
            branch: "b".into(),
            incarnation: 1,
            kind,
            provider: provider.into(),
            name: name.into(),
            turn: Some(2),
            detail: "{}".into(),
            used_ms,
        };
        let store = yard.store();
        for r in [
            row("newer", SandboxKind::Kept, "microsandbox", 20),
            row("older", SandboxKind::Kept, "microsandbox", 10),
            row("snap", SandboxKind::Snapshot, "microsandbox", 15),
            row("elsewhere", SandboxKind::Kept, "substrate:http://x", 5),
        ] {
            store.sandboxes().put_sandbox(&r).unwrap();
        }
        let held = held_sandboxes(&yard, &SandboxOptions::default()).unwrap();
        let names: Vec<&str> = held.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["older", "snap", "newer"]);
        assert_eq!(held[1].kind, HeldKind::Snapshot);
        assert_eq!(held[0].kind, HeldKind::Kept);
        assert_eq!(held[0].turn, Some(2));
    }

    #[test]
    fn a_configured_price_wins_over_the_catalog_and_an_unknown_model_has_none() {
        let mut config = Config::default();
        assert_eq!(
            price_source(&config, Api::Anthropic, "claude-sonnet-4-6"),
            Some(PriceSource::Catalog)
        );
        assert_eq!(price_source(&config, Api::Anthropic, "mystery-1"), None);
        config.prices.insert(
            "mystery-*".into(),
            Price {
                input: 1.0,
                output: 2.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
        );
        assert_eq!(
            price_source(&config, Api::Anthropic, "mystery-1"),
            Some(PriceSource::Configured("mystery-*".into()))
        );
        assert_eq!(
            price_source(&config, Api::Anthropic, "claude-sonnet-4-6"),
            Some(PriceSource::Catalog)
        );
    }

    #[test]
    fn confinement_reports_a_reason_or_works() {
        // Hosts differ; either answer is a valid one, a reason is not empty.
        if let Err(reason) = egress_confinement() {
            assert!(!reason.is_empty());
        }
    }
}
