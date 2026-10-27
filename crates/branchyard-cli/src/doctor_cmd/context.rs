//! What the checks share: where the repository is, how its configuration
//! loads, and the store, opened at most once and only when opening it
//! changes nothing the user did not already allow.
//!
//! Everything here is computed once and kept, so a check that needs the
//! configuration or the yard does not read it again, and a check that
//! cannot run says why instead of failing a second time.

use std::cell::OnceCell;
use std::path::{Path, PathBuf};

use branchyard::doctor::{store_schema, SchemaState};
use branchyard::inventory::Inventory;
use branchyard::Yard;
use branchyard_setup::config::ProjectConfig;
use branchyard_workspace::{GitError, Repository};

use crate::args::Globals;
use crate::setup_io::{self, Located};

/// Reads a variable by name.
pub(crate) type Vars = Box<dyn Fn(&str) -> Option<String>>;

/// One configuration file, as it loads.
pub(crate) enum Layer {
    /// There is no such file.
    Absent,
    /// The file loads and validates.
    Loaded,
    /// The loader refused the file; the message names the file and, when
    /// it knows, the line.
    Refused(String),
}

/// The user file and the project file, and the two merged with the
/// `BRANCHYARD_*` variables, as `by config show` reads them.
pub(crate) struct Configuration {
    /// Where the files are.
    pub located: Located,
    /// The user file.
    pub user: Layer,
    /// The project file.
    pub project: Layer,
    /// The merged result.
    pub merged: Merged,
}

/// The merged configuration, or why there is none.
pub(crate) enum Merged {
    /// The files merged (an empty configuration when there are none).
    Ready(Box<ProjectConfig>),
    /// A file above did not load, so nothing was merged.
    Blocked,
    /// Both files load but do not merge, or a variable is refused.
    Refused(String),
}

/// The repository's store, as far as `by doctor` may go.
pub(crate) enum Store {
    /// Opened, as every `by` command opens it.
    Open(Box<Yard>),
    /// There is no repository to look in.
    NoRepository,
    /// The repository has no `.branchyard/` yet.
    Absent,
    /// Not opened on purpose; the reason says why.
    Withheld(String),
    /// Opening failed with this message.
    Failed(String),
}

/// What the checks read.
pub(crate) struct Context {
    /// Where the search for the repository started.
    pub base: PathBuf,
    /// The repository's top level, or why there is none.
    pub repository: Result<PathBuf, String>,
    /// The configuration files.
    pub configuration: Configuration,
    /// The global flags and variables, with `[remote]` filled in from the
    /// configuration where they left it unset, as every command does.
    pub globals: Globals,
    /// Variables, by name.
    pub vars: Vars,
    store: OnceCell<Store>,
    inventory: OnceCell<Result<Inventory, String>>,
    hooks: OnceCell<Result<Vec<super::hooks::Hooked>, String>>,
}

impl Context {
    /// The context for `path`, or for the repository `by` would act on from
    /// here: the inherited `BRANCHYARD_ROOT` inside a harness, else the
    /// current directory.
    pub(crate) fn new(path: Option<&str>, globals: &Globals) -> Result<Context, std::io::Error> {
        let vars: Vars = Box::new(|name| std::env::var(name).ok());
        let cwd = std::env::current_dir()?;
        let base = match path {
            Some(path) => Ok(cwd.join(path)),
            None => match crate::inherited::root() {
                Ok(Some(root)) => Ok(root),
                Ok(None) => Ok(cwd),
                Err(refusal) => Err((cwd, refusal.to_string())),
            },
        };
        Ok(match base {
            Ok(base) => Context::at(base, globals, vars),
            Err((cwd, why)) => Context::build(cwd, Err(why), globals, vars),
        })
    }

    /// The context for the directory `base`, reading variables from `vars`.
    pub(crate) fn at(base: PathBuf, globals: &Globals, vars: Vars) -> Context {
        let repository = find_repository(&base);
        Context::build(base, repository, globals, vars)
    }

    fn build(
        base: PathBuf,
        repository: Result<PathBuf, String>,
        globals: &Globals,
        vars: Vars,
    ) -> Context {
        let configuration = load_configuration(&base, &vars);
        let mut globals = globals.clone();
        if let Merged::Ready(config) = &configuration.merged {
            crate::defaults::apply_globals(config, &mut globals);
        }
        Context {
            base,
            repository,
            configuration,
            globals,
            vars,
            store: OnceCell::new(),
            inventory: OnceCell::new(),
            hooks: OnceCell::new(),
        }
    }

    /// The merged configuration, when the files load and merge.
    pub(crate) fn config(&self) -> Option<&ProjectConfig> {
        match &self.configuration.merged {
            Merged::Ready(config) => Some(config),
            Merged::Blocked | Merged::Refused(_) => None,
        }
    }

    /// The repository's top level, when there is one.
    pub(crate) fn root(&self) -> Option<&Path> {
        self.repository.as_deref().ok()
    }

    /// Whether the variable `name` is set to something.
    pub(crate) fn var_set(&self, name: &str) -> bool {
        (self.vars)(name).is_some_and(|v| !v.is_empty())
    }

    /// The harnesses this machine has, as `by harnesses` shows them (from
    /// its cache), detected the first time it is asked for.
    pub(crate) fn inventory(&self) -> &Result<Inventory, String> {
        self.inventory
            .get_or_init(crate::harness_cmd::detected_here)
    }

    /// The harnesses on `PATH` whose hooks are installed or were heard,
    /// read the first time it is asked for.
    pub(crate) fn hooks(&self) -> &Result<Vec<super::hooks::Hooked>, String> {
        self.hooks.get_or_init(|| super::hooks::hooked(&self.base))
    }

    /// The repository's store, opened the first time it is asked for.
    pub(crate) fn store(&self) -> &Store {
        self.store.get_or_init(|| match &self.repository {
            Ok(root) => open_store(root),
            Err(_) => Store::NoRepository,
        })
    }
}

/// The top level of the git working tree containing `base`, or why `base`
/// is not in one.
fn find_repository(base: &Path) -> Result<PathBuf, String> {
    if !base.is_dir() {
        return Err(format!("{} is not a directory", base.display()));
    }
    match Repository::open(base) {
        Ok(repository) => Ok(repository.root().to_path_buf()),
        Err(GitError::NotAWorkTree(path)) => {
            Err(format!("{} is not inside a git repository", path.display()))
        }
        Err(GitError::Spawn(error)) => Err(format!("cannot run git: {error}")),
        Err(other) => Err(format!("git could not read {}: {other}", base.display())),
    }
}

/// Read both files, then merge them as `by config show` does.
fn load_configuration(base: &Path, vars: &Vars) -> Configuration {
    let located = setup_io::locate(base);
    let layer = |path: &Path, exists: bool| match exists {
        false => Layer::Absent,
        true => match setup_io::read_layer(path) {
            Ok(_) => Layer::Loaded,
            Err(error) => Layer::Refused(error.to_string()),
        },
    };
    let user = layer(&located.user, located.user.is_file());
    let project = layer(&located.project, located.project_exists);
    let merged = match (&user, &project) {
        (Layer::Refused(_), _) | (_, Layer::Refused(_)) => Merged::Blocked,
        _ => match setup_io::load(base, Some(vars.as_ref())) {
            Ok(effective) => Merged::Ready(Box::new(effective.config)),
            Err(error) => Merged::Refused(error.to_string()),
        },
    };
    Configuration {
        located,
        user,
        project,
        merged,
    }
}

/// Open the repository at `root`, as every `by` command does, unless that
/// would change what the user keeps: `by doctor` creates no `.branchyard/`
/// and does not upgrade an older store.
fn open_store(root: &Path) -> Store {
    if !root.join(".branchyard").is_dir() {
        return Store::Absent;
    }
    match store_schema(root) {
        Err(error) => return Store::Failed(error.to_string()),
        Ok(Some(schema)) => match schema.state() {
            SchemaState::Current | SchemaState::Unrecorded => {}
            SchemaState::Upgradable => {
                return Store::Withheld(format!(
                    "the store is at schema {}, which opening upgrades in place; doctor does not \
                     upgrade it (any other `by` command does)",
                    schema.found.unwrap_or_default()
                ))
            }
            SchemaState::Newer | SchemaState::Unsupported => {
                return Store::Withheld(format!(
                    "the store is at schema {}, which this binary (schema {}) does not read",
                    schema.found.unwrap_or_default(),
                    schema.supported
                ))
            }
        },
        Ok(None) => {}
    }
    match Yard::open(root) {
        Ok(yard) => Store::Open(Box::new(yard)),
        Err(error) => Store::Failed(error.to_string()),
    }
}
