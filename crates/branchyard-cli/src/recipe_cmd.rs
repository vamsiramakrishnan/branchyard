//! `by recipe list|show|trust|untrust|check`: environment recipes, the
//! `[recipes.NAME]` scripts that create a machine, and the trust decision
//! they go through before any of them runs. See docs/recipes.md.
//!
//! Trust works as for `[workspace]` (docs/workspace.md): a repository's
//! recipe is code you have not read, so none of its commands runs until you
//! trust it, in the same per-user trust file, under the key
//! `<canonical root>#recipe:<name>`, for the SHA-256 of its commands; a
//! change asks again. A recipe in your own user file needs no trust and
//! replaces the repository's of the same name. On a terminal, `by recipe
//! check` asks once; otherwise it refuses and points to `by recipe trust`.
//! A harness on a branch can never trust anything.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use branchyard_recipe::{static_checks, Check, Mode, Recipe, RecipeProvider, Status};
use branchyard_sandbox::{ExecSpec, SandboxProvider, SandboxSpec};
use branchyard_setup::config::RecipeConfig;
use serde::Serialize;
use serde_json::json;

use crate::commands::{print, Env, Failure, Outcome, Target};
use crate::{setup_io, workspace_cmd};

pub const RECIPE_EXAMPLES: &str = "\
Examples:
  by recipe list
  by recipe trust devbox                # after reading its commands with by recipe show
  by recipe check devbox                # doctor, then create, exec, (suspend, resume,) destroy
  by recipe check devbox --no-smoke     # only the doctor

[recipes.devbox] in branchyard.toml:
  create  = \"./scripts/vm/create.sh\"    # prints {\"schemaVersion\":1,\"connection\":{...}}
  suspend = \"./scripts/vm/suspend.sh\"
  resume  = \"./scripts/vm/resume.sh\"
  destroy = \"./scripts/vm/destroy.sh\"
  doctor  = \"./scripts/vm/doctor.sh\"";

/// `by recipe ...`.
#[derive(clap::Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum RecipeAction {
    /// Every recipe here: where it is defined, whether it is trusted, whether it can pause
    List,
    /// One recipe's commands, where it is defined, its digest and whether it is trusted
    Show { name: String },
    /// Trust a repository recipe's commands as they are now
    Trust { name: String },
    /// Forget the trust decision for a repository recipe
    Untrust { name: String },
    /// Run the recipe's doctor, then create a machine, exec in it, suspend and resume it when
    /// it can, and destroy it
    Check {
        name: String,
        /// Only the doctor: create nothing
        #[arg(long)]
        no_smoke: bool,
    },
}

/// Where a recipe is defined.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Origin {
    Project,
    User,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Trust {
    NotNeeded,
    Trusted,
    Untrusted,
    Changed,
}

struct Resolved {
    name: String,
    config: RecipeConfig,
    origin: Origin,
    file: PathBuf,
    root: PathBuf,
    digest: String,
    trust: Trust,
}

impl Resolved {
    fn key(&self) -> String {
        trust_key(&self.root, &self.name)
    }

    fn recipe(&self) -> Recipe {
        let mut recipe = Recipe::new(&self.name, &self.root, &self.config.create)
            .with_destroy(self.config.destroy.as_deref());
        recipe.suspend = self.config.suspend.clone();
        recipe.resume = self.config.resume.clone();
        recipe.doctor = self.config.doctor.clone();
        if let Some(seconds) = self.config.timeout_seconds {
            recipe.timeout = Duration::from_secs(seconds);
        }
        recipe
    }

    fn runs_ok(&self) -> bool {
        matches!(self.trust, Trust::NotNeeded | Trust::Trusted)
    }
}

fn trust_key(root: &Path, name: &str) -> String {
    format!("{}#recipe:{name}", root.display())
}

fn root() -> Result<PathBuf, Failure> {
    let cwd = std::env::current_dir()?;
    let root = setup_io::project_root(&cwd);
    Ok(std::fs::canonicalize(&root).unwrap_or(root))
}

fn read(path: &Path) -> Result<Option<branchyard_setup::config::ProjectConfig>, Failure> {
    match path.is_file() {
        false => Ok(None),
        true => setup_io::read_layer(path).map(Some).map_err(|e| {
            Failure::Message(format!(
                "{e}\n(fix it, or check it with `by config validate`)"
            ))
        }),
    }
}

/// Every recipe for the repository at `root`, by name: the user file's
/// over the repository's.
fn resolve_all(root: &Path) -> Result<BTreeMap<String, Resolved>, Failure> {
    let project_file = root.join(branchyard_setup::config::PROJECT_FILE);
    let user_file = setup_io::user_file();
    let mut all = BTreeMap::new();
    for (origin, file) in [(Origin::Project, project_file), (Origin::User, user_file)] {
        let Some(config) = read(&file)? else { continue };
        for (name, recipe) in config.recipes {
            let digest = recipe.digest();
            let trust = match origin {
                Origin::User => Trust::NotNeeded,
                Origin::Project => match workspace_cmd::trusted_digest(&trust_key(root, &name))? {
                    Some(trusted) if trusted == digest => Trust::Trusted,
                    Some(_) => Trust::Changed,
                    None => Trust::Untrusted,
                },
            };
            all.insert(
                name.clone(),
                Resolved {
                    name,
                    config: recipe,
                    origin,
                    file: file.clone(),
                    root: root.to_path_buf(),
                    digest,
                    trust,
                },
            );
        }
    }
    Ok(all)
}

fn resolve(root: &Path, name: &str) -> Result<Resolved, Failure> {
    let mut all = resolve_all(root)?;
    let names: Vec<String> = all.keys().cloned().collect();
    all.remove(name).ok_or_else(|| {
        Failure::Message(match names.is_empty() {
            true => format!(
                "no recipe named {name}: {} has no [recipes] (docs/recipes.md)",
                root.display()
            ),
            false => format!("no recipe named {name}; there are {}", names.join(", ")),
        })
    })
}

fn describe(r: &Resolved) -> String {
    let mut text = format!(
        "recipe {} ({}) runs, as you, in {}:\n",
        r.name,
        r.file.display(),
        r.root.display()
    );
    for (what, command) in [
        ("create", Some(&r.config.create)),
        ("suspend", r.config.suspend.as_ref()),
        ("resume", r.config.resume.as_ref()),
        ("destroy", r.config.destroy.as_ref()),
        ("doctor", r.config.doctor.as_ref()),
    ] {
        if let Some(command) = command {
            text.push_str(&format!("  {what:<8} {command}\n"));
        }
    }
    text
}

fn in_harness() -> Result<(), Failure> {
    match workspace_cmd::in_harness() {
        Some(_) => Err(Failure::Message(
            "a harness running on a branch cannot run or trust recipes; that is a person's \
             decision, made outside any branch"
                .into(),
        )),
        None => Ok(()),
    }
}

/// Require that `r`'s commands may run: trusted, or trusted now on the
/// terminal.
fn require_trust(env: &Env, r: &Resolved) -> Result<(), Failure> {
    if r.runs_ok() {
        return Ok(());
    }
    let changed = match r.trust {
        Trust::Changed => " It changed since you last trusted it.",
        _ => "",
    };
    if env.stdin_tty && env.stderr_tty {
        eprint!("{}", describe(r));
        let answer = crate::console::terminal_prompt(&format!(
            "by: trust recipe {}?{changed} [y/N] ",
            r.name
        ))?;
        if matches!(answer.trim(), "y" | "Y" | "yes" | "Yes") {
            return workspace_cmd::record_trust(&r.key(), &r.digest);
        }
        return Err(Failure::Message(format!(
            "not trusted; nothing ran. Change or remove [recipes.{}] in {}, or trust it with \
             `by recipe trust {}`",
            r.name,
            r.file.display(),
            r.name
        )));
    }
    let why = match r.trust {
        Trust::Changed => "changed since you trusted it",
        _ => "has commands you have not trusted",
    };
    Err(Failure::Message(format!(
        "recipe {} in {} {why}, and there is no terminal to ask on; nothing ran. Review it with \
         `by recipe show {}`, then run `by recipe trust {}`",
        r.name,
        r.file.display(),
        r.name,
        r.name
    )))
}

/// Recipe `name` of the repository here, once its commands may run: not
/// inside a harness, and trusted (asking on a terminal). For other
/// commands that make a machine from a recipe, such as `by harnesses --on
/// recipe:NAME`.
pub(crate) fn trusted(env: &Env, name: &str) -> Result<Recipe, Failure> {
    in_harness()?;
    let r = resolve(&root()?, name)?;
    require_trust(env, &r)?;
    Ok(r.recipe())
}

/// `by recipe ACTION`.
pub fn main(env: &Env, target: &Target, action: &RecipeAction, json: bool) -> Outcome {
    if let Target::Remote(_) = target {
        return Err(Failure::Message(
            "by recipe runs a repository's recipe scripts on this machine; it does not take \
             --remote"
                .into(),
        ));
    }
    let root = root()?;
    match action {
        RecipeAction::List => list(&root, json),
        RecipeAction::Show { name } => show(&resolve(&root, name)?, json),
        RecipeAction::Trust { name } => {
            in_harness()?;
            let r = resolve(&root, name)?;
            if r.origin == Origin::User {
                return print(&format!(
                    "recipe {name} is in your own {}; it needs no trust\n",
                    r.file.display()
                ));
            }
            workspace_cmd::record_trust(&r.key(), &r.digest)?;
            match json {
                true => print(&format!(
                    "{:#}\n",
                    json!({ "recipe": name, "trusted": true, "digest": r.digest })
                )),
                false => print(&format!(
                    "{}trusted recipe {name} as it is now (digest {}); a change to it asks again\n",
                    describe(&r),
                    &r.digest[..12]
                )),
            }
        }
        RecipeAction::Untrust { name } => {
            let removed = workspace_cmd::forget_trust(&trust_key(&root, name))?;
            match json {
                true => print(&format!(
                    "{:#}\n",
                    json!({ "recipe": name, "removed": removed })
                )),
                false if removed => print(&format!("recipe {name} is no longer trusted\n")),
                false => print(&format!("recipe {name} was not trusted\n")),
            }
        }
        RecipeAction::Check { name, no_smoke } => {
            in_harness()?;
            let r = resolve(&root, name)?;
            require_trust(env, &r)?;
            check(&r, !no_smoke, json)
        }
    }
}

fn list(root: &Path, json: bool) -> Outcome {
    let all = resolve_all(root)?;
    if json {
        let entries: Vec<_> = all
            .values()
            .map(|r| {
                json!({
                    "name": r.name,
                    "origin": r.origin,
                    "file": r.file,
                    "trust": r.trust,
                    "pause": r.recipe().can_pause(),
                    "description": r.config.description,
                })
            })
            .collect();
        return print(&format!("{:#}\n", json!(entries)));
    }
    if all.is_empty() {
        return print(&format!(
            "no recipes: add [recipes.NAME] to {} (docs/recipes.md)\n",
            root.join(branchyard_setup::config::PROJECT_FILE).display()
        ));
    }
    let mut text = String::new();
    for r in all.values() {
        let trust = match r.trust {
            Trust::NotNeeded => "yours",
            Trust::Trusted => "trusted",
            Trust::Untrusted => "untrusted",
            Trust::Changed => "changed since trusted",
        };
        let pause = match r.recipe().can_pause() {
            true => "exec, pause",
            false => "exec",
        };
        text.push_str(&format!(
            "{:<16} {:<22} {:<12} {}\n",
            r.name,
            trust,
            pause,
            r.config.description.as_deref().unwrap_or("")
        ));
    }
    print(&text)
}

fn show(r: &Resolved, json: bool) -> Outcome {
    if json {
        return print(&format!(
            "{:#}\n",
            json!({
                "name": r.name,
                "origin": r.origin,
                "file": r.file,
                "root": r.root,
                "digest": r.digest,
                "trust": r.trust,
                "recipe": r.config,
            })
        ));
    }
    let trust = match r.trust {
        Trust::NotNeeded => "not needed (your own file)",
        Trust::Trusted => "trusted",
        Trust::Untrusted => "not trusted: by recipe trust NAME",
        Trust::Changed => "changed since you trusted it: by recipe trust NAME",
    };
    print(&format!(
        "{}  digest   {}\n  trust    {}\n",
        describe(r),
        &r.digest[..12],
        trust.replace("NAME", &r.name)
    ))
}

fn push(checks: &mut Vec<Check>, id: &str, status: Status, message: String) {
    checks.push(Check {
        id: id.into(),
        status,
        message,
        remediation: None,
    });
}

/// The doctor, then a smoke test of the provider over the recipe.
fn check(r: &Resolved, smoke: bool, json: bool) -> Outcome {
    let recipe = r.recipe();
    let mut checks = static_checks(&recipe);
    if let Some(doctor) = &recipe.doctor {
        let ran = branchyard_recipe::run(&recipe, doctor, Mode::Doctor, "doctor", None)?;
        let said = ran.stdout.trim().lines().last().unwrap_or("").to_owned();
        match ran.success() {
            true => push(
                &mut checks,
                "recipe.doctor.run",
                Status::Pass,
                match said.is_empty() {
                    true => "the doctor command passed".into(),
                    false => format!("the doctor command passed: {said}"),
                },
            ),
            false => push(
                &mut checks,
                "recipe.doctor.run",
                Status::Fail,
                ran.failure(&recipe.name, Mode::Doctor),
            ),
        }
    }
    let healthy = checks.iter().all(|c| c.status != Status::Fail);
    if smoke && healthy {
        smoke_test(&recipe, &mut checks);
    } else if smoke {
        push(
            &mut checks,
            "smoke",
            Status::Warn,
            "skipped: the doctor failed".into(),
        );
    }
    let ok = checks.iter().all(|c| c.status != Status::Fail);
    if json {
        print(&format!(
            "{:#}\n",
            json!({ "recipe": r.name, "ok": ok, "checks": checks })
        ))?;
    } else {
        let mut text = String::new();
        for c in &checks {
            text.push_str(&format!("{:<5} {:<32} {}\n", c.status, c.id, c.message));
            if let (Some(fix), true) = (&c.remediation, c.status != Status::Pass) {
                text.push_str(&format!("      {:<32} {fix}\n", ""));
            }
        }
        text.push_str(match ok {
            true => "recipe ok\n",
            false => "recipe failed\n",
        });
        print(&text)?;
    }
    match ok {
        true => Ok(()),
        false => Err(Failure::Reported),
    }
}

/// The provider options `--provider recipe:NAME` names: the recipe as
/// resolved here, refused unless it may run (trusted as it is now, or your
/// own), and never from a harness on a branch. Its commands are stored
/// with the branch, so later turns, recovery and removal run what was
/// trusted now.
pub fn provider(args: &crate::args::RecipeArgs) -> Result<branchyard::RecipeOptions, Failure> {
    in_harness()?;
    let root = root()?;
    let r = resolve(&root, &args.name)?;
    if !r.runs_ok() {
        let why = match r.trust {
            Trust::Changed => "changed since you trusted it",
            _ => "has commands you have not trusted",
        };
        return Err(Failure::Message(format!(
            "recipe {} in {} {why}; nothing ran. Review it with `by recipe show {}`, then run \
             `by recipe trust {}`",
            r.name,
            r.file.display(),
            r.name,
            r.name
        )));
    }
    Ok(branchyard::RecipeOptions {
        name: r.name.clone(),
        create: r.config.create.clone(),
        suspend: r.config.suspend.clone(),
        resume: r.config.resume.clone(),
        destroy: r.config.destroy.clone(),
        timeout_seconds: r.config.timeout_seconds,
        digest: r.digest.clone(),
        workdir: args.workdir.clone().unwrap_or_default(),
        home: args.home.clone().unwrap_or_default(),
        pass_env: args.pass_env.clone(),
        keep: args.lifecycle.keep.unwrap_or_default(),
        max_paused: args.lifecycle.max_paused,
    })
}

fn smoke_test(recipe: &Recipe, checks: &mut Vec<Check>) {
    let ssh = std::env::var("BRANCHYARD_SSH")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "ssh".into());
    let provider = RecipeProvider::new(recipe.clone(), ssh);
    let name = format!("by-check-{}-{}", recipe.name, std::process::id());
    match provider.ensure(&SandboxSpec::new(&name)) {
        Ok(_) => {
            let how = provider
                .transport(&name)
                .map(|t| t.describe())
                .unwrap_or_default();
            push(
                checks,
                "smoke.create",
                Status::Pass,
                format!("created {name}, reached by {how}"),
            );
        }
        Err(e) => {
            push(checks, "smoke.create", Status::Fail, e.to_string());
            return;
        }
    }
    let exec = |checks: &mut Vec<Check>, id: &str| {
        let spec = ExecSpec {
            argv: vec!["sh".into(), "-c".into(), "pwd && uname -sm".into()],
            ..ExecSpec::default()
        };
        let outcome = provider.exec(&name, &spec).and_then(|mut process| {
            drop(process.take_stdin());
            let mut out = String::new();
            if let Some(mut stdout) = process.take_stdout() {
                let _ = std::io::Read::read_to_string(&mut stdout, &mut out);
            }
            let status = process.wait()?;
            Ok((status, out))
        });
        match outcome {
            Ok((status, out)) if status.success() => push(
                checks,
                id,
                Status::Pass,
                format!("ran in {}", out.trim().replace('\n', " on ")),
            ),
            Ok((status, _)) => push(
                checks,
                id,
                Status::Fail,
                format!("exec ended with {status}"),
            ),
            Err(e) => push(checks, id, Status::Fail, e.to_string()),
        }
    };
    exec(checks, "smoke.exec");
    if recipe.can_pause() {
        match provider.pause(&name).and_then(|()| provider.resume(&name)) {
            Ok(_) => {
                push(
                    checks,
                    "smoke.suspend_resume",
                    Status::Pass,
                    "suspended and resumed".into(),
                );
                exec(checks, "smoke.exec_after_resume");
            }
            Err(e) => push(checks, "smoke.suspend_resume", Status::Fail, e.to_string()),
        }
    }
    match (provider.destroy(&name), recipe.destroy.is_some()) {
        (Ok(()), true) => push(
            checks,
            "smoke.destroy",
            Status::Pass,
            format!("destroyed {name}"),
        ),
        (Ok(()), false) => push(
            checks,
            "smoke.destroy",
            Status::Warn,
            format!("{name} was left running: the recipe has no destroy"),
        ),
        (Err(e), _) => push(checks, "smoke.destroy", Status::Fail, e.to_string()),
    }
}
