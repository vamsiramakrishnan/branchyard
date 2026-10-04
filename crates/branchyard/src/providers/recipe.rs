//! [`crate::Provider::Recipe`]: a machine an environment recipe's scripts
//! make, reached over ssh or the recipe's exec command, the worktree and
//! the private home copied in and back.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use branchyard_recipe::{Recipe, RecipeProvider};
use branchyard_sandbox::{SandboxProvider, SandboxSpec};
use branchyard_substrate::transfer;

use super::ProviderKind;
use crate::placement::{pull_staged, said_back, staging, Placement, SandboxPlan};
use crate::snapshots::Lifecycle;
use crate::state::{Fence, Record};
use crate::{Error, RecipeOptions, Yard};

impl ProviderKind for RecipeOptions {
    fn name(&self) -> &'static str {
        "recipe"
    }

    fn key(&self) -> String {
        format!("recipe:{}", self.name)
    }

    /// A recipe's machine has no checkpoints to keep.
    fn lifecycle(&self) -> Option<Lifecycle> {
        Some(Lifecycle::of(self.keep, Some(0), self.max_paused))
    }

    fn service_kind(&self) -> &'static str {
        crate::services::KIND_RECIPE_MACHINE
    }

    fn check(&self, _: &Yard) -> Result<(), Error> {
        check_recipe(self)
    }

    fn guest_paths(&self, record: &Record) -> (String, String) {
        recipe_paths(record, self)
    }

    fn prepare(
        &self,
        yard: &Yard,
        record: &Record,
        fence: &Fence,
        plan: &SandboxPlan,
    ) -> Result<Placement, String> {
        Placement::recipe(yard, record, fence, self, plan)
    }

    fn recover(&self, yard: &Yard, record: &Record, sandbox: &str) -> Option<String> {
        Some(recover_machine(yard, record, self, sandbox))
    }

    fn open(&self, yard: &Yard) -> Result<Arc<dyn SandboxProvider>, String> {
        Ok(recipe_provider(yard, self))
    }

    fn destroy(&self, yard: &Yard, sandbox: &str) -> Result<String, String> {
        let recipes = recipe_provider(yard, self);
        if recipes.result(sandbox).is_none() {
            return Ok(format!("machine {sandbox} was already gone"));
        }
        SandboxProvider::destroy(recipes.as_ref(), sandbox)
            .map_err(|e| format!("could not destroy machine {sandbox}: {e}"))?;
        Ok(format!(
            "destroyed machine {sandbox} (recipe {})",
            self.name
        ))
    }
}

fn check_recipe(options: &RecipeOptions) -> Result<(), Error> {
    let refuse = |why: String| {
        Err(Error::Unsupported(format!(
            "the recipe provider (recipe {}) {why}",
            options.name
        )))
    };
    if options.name.trim().is_empty() {
        return Err(Error::Unsupported(
            "the recipe provider needs a recipe's name".into(),
        ));
    }
    if options.create.trim().is_empty() {
        return refuse("has no create command".into());
    }
    for (what, path) in [("workdir", &options.workdir), ("home", &options.home)] {
        if !path.is_empty() && !Path::new(path).is_absolute() {
            return refuse(format!("needs an absolute {what}, not {path:?}"));
        }
    }
    if options.keep == crate::SandboxKeep::Pause
        && (options.suspend.is_none() || options.resume.is_none())
    {
        return refuse(
            "cannot keep its machine paused between turns: it needs both suspend and resume".into(),
        );
    }
    Ok(())
}

/// A recipe branch's worktree and home on its machine.
fn recipe_paths(record: &Record, options: &RecipeOptions) -> (String, String) {
    let (branch, worktree) = (&record.info.name, &record.info.worktree);
    (
        options.workdir(branch, worktree),
        options.home(branch, worktree),
    )
}

/// The provider for a recipe branch: its machines' records in the store's
/// `recipes` directory, shared by every process on this repository, and
/// `$BRANCHYARD_SSH` (default `ssh`) as the ssh program.
pub(crate) fn recipe_provider(yard: &Yard, options: &RecipeOptions) -> Arc<RecipeProvider> {
    let mut recipe = Recipe::new(&options.name, &yard.root, &options.create)
        .with_destroy(options.destroy.as_deref());
    recipe.suspend = options.suspend.clone();
    recipe.resume = options.resume.clone();
    if let Some(seconds) = options.timeout_seconds {
        recipe.timeout = Duration::from_secs(seconds);
    }
    let ssh = std::env::var("BRANCHYARD_SSH")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "ssh".into());
    Arc::new(RecipeProvider::new(recipe, ssh).with_state_dir(yard.store().dir().join("recipes")))
}

/// Bring back what the harness left on the recipe machine a stopped
/// engine's turn journaled, if the machine is still recorded: what still
/// runs there is stopped first, then the worktree (only if the host's still
/// holds exactly what was sent) and the home come back, and the machine is
/// destroyed through the recipe. Returns what recovery should report.
fn recover_machine(yard: &Yard, record: &Record, options: &RecipeOptions, machine: &str) -> String {
    // A machine's name is `by-<branch>-<ms>`; a branch name may hold dots.
    let stage = (!machine.is_empty() && !machine.contains('/') && !machine.starts_with('.'))
        .then(|| staging(yard, machine));
    let provider = recipe_provider(yard, options);
    let mut said = Vec::new();
    let existed = matches!(provider.inspect(machine), Ok(Some(_)));
    if existed {
        let pulled = (|| {
            let stage = stage
                .as_deref()
                .ok_or("its machine's name cannot name a staging directory")?;
            // The harness outlives a dead engine's ssh connection: stop it,
            // then make the machine usable for the transfer again.
            provider.stop(machine).map_err(|e| e.to_string())?;
            provider
                .ensure(&SandboxSpec::new(machine))
                .map_err(|e| e.to_string())?;
            let (workdir, home) = recipe_paths(record, options);
            let endpoint = transfer::Exec::new(provider.as_ref(), machine);
            pull_staged(
                &endpoint,
                record,
                (Path::new(&workdir), Path::new(&home)),
                stage,
            )
        })();
        said.push(said_back(pulled, "machine", machine));
    }
    let destroyed = provider.destroy(machine);
    if let Some(stage) = &stage {
        let _ = std::fs::remove_dir_all(stage);
    }
    said.push(match (destroyed, existed) {
        (Ok(()), true) => format!("destroyed its machine {machine} (recipe {})", options.name),
        (Ok(()), false) => format!("its machine {machine} was already gone"),
        (Err(error), _) => format!("could not destroy its machine {machine}: {error}"),
    });
    said.join("; ")
}
