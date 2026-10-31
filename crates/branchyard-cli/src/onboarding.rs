//! Repository and compute choices for the installer's guided setup journey.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use branchyard_setup::probe::Facts;
use branchyard_setup::Topic;
use serde_json::Value;

use crate::args::SetupFlags;
use crate::cloud_setup::CloudKind;
use crate::commands::{Env, Failure};
use crate::errors::Report;

/// Select the working directory and initialize a repository only with consent.
pub(crate) fn directory(env: &Env, args: &SetupFlags) -> Result<(), Failure> {
    let interactive = env.stdin_tty && env.stdout_tty && !args.json;
    if args.guided && !interactive && !args.yes && !args.dry_run {
        return Err(Report::new("manual", "guided setup needs a terminal")
            .next("run by setup --guided in your terminal")
            .next("by setup --guided --yes --directory PATH")
            .into());
    }
    let mut directory = args.directory.clone().unwrap_or(std::env::current_dir()?);
    if !directory.is_absolute() {
        directory = std::env::current_dir()?.join(directory);
    }
    let has_repo = crate::setup_io::project_root(&directory)
        .join(".git")
        .exists();
    if args.guided && !args.no_config && !has_repo && interactive && !args.yes && !args.dry_run {
        let selected: String = cliclack::input("Which project should Branchyard set up?")
            .default_input(&directory.display().to_string())
            .validate(|path: &String| {
                if path.trim().is_empty() {
                    Err("Choose a project directory")
                } else {
                    Ok(())
                }
            })
            .interact()
            .map_err(crate::wizard::cancelled)?;
        directory = PathBuf::from(selected);
        if !directory.is_absolute() {
            directory = std::env::current_dir()?.join(directory);
        }
    }
    if args.guided
        && !args.no_config
        && !crate::setup_io::project_root(&directory)
            .join(".git")
            .exists()
        && !args.dry_run
    {
        if args.yes && args.directory.is_none() {
            return Err(Report::new(
                "not_a_repository",
                "the current directory is not a git repository",
            )
            .next("choose a project with by setup --guided --yes --directory PATH")
            .into());
        }
        let approved = args.yes
            || cliclack::confirm(format!(
                "Initialize a git repository in {}?",
                directory.display()
            ))
            .initial_value(true)
            .interact()
            .map_err(crate::wizard::cancelled)?;
        if !approved {
            return Err(Report::new(
                "manual",
                "repository initialization was cancelled; nothing was written",
            )
            .next("choose an existing repository with by setup --guided --directory PATH")
            .into());
        }
        std::fs::create_dir_all(&directory)?;
        let status = Command::new("git")
            .args(["init", "--quiet"])
            .arg(&directory)
            .stdin(Stdio::null())
            .status()
            .map_err(|error| {
                Report::new(
                    "tool_unavailable",
                    format!("could not initialize {}: {error}", directory.display()),
                )
                .next("install git, then rerun by setup --guided --directory PATH")
            })?;
        if !status.success() {
            return Err(Report::new(
                "git",
                format!("git could not initialize {}", directory.display()),
            )
            .next("check this directory is writable, then rerun by setup --guided --directory PATH")
            .into());
        }
    }
    std::env::set_current_dir(&directory).map_err(|error| {
        Report::new(
            "io",
            format!(
                "could not open project directory {}: {error}",
                directory.display()
            ),
        )
        .next("choose an existing directory with by setup --directory PATH")
        .into()
    })
}

/// Seed the existing project interview with guided compute choices.
pub(crate) fn answers(
    env: &Env,
    args: &SetupFlags,
    facts: &Facts,
) -> Result<BTreeMap<String, Value>, Failure> {
    let mut answers = BTreeMap::new();
    if args.guided && facts.project_config && !args.dry_run {
        crate::cloud_setup::refresh_saved(&std::env::current_dir()?)?;
    }
    if !args.guided || args.no_config || facts.project_config {
        return Ok(answers);
    }
    let interactive = env.stdin_tty && env.stdout_tty && !args.json && !args.yes && !args.dry_run;
    let backend = if interactive {
        cliclack::select("Where should agent tasks run?")
            .initial_value("local")
            .item(
                "local",
                "This machine",
                "start with the tools already installed here",
            )
            .item(
                "microsandbox",
                "Local microVM",
                "Linux, KVM and the runtime-enabled build",
            )
            .item(
                "substrate",
                "Existing Agent Substrate",
                "connect to your cluster and bridge template",
            )
            .item(
                "gke",
                "New GKE deployment",
                "review and provision dedicated cloud resources",
            )
            .item(
                "gce",
                "New GCE worker",
                "review and provision a dedicated remote machine",
            )
            .item(
                "remote",
                "Existing remote environment",
                "select a saved deployment, HTTPS server or SSH worker",
            )
            .interact()
            .map_err(crate::wizard::cancelled)?
    } else {
        "local"
    };
    let saved = match interactive && matches!(backend, "remote" | "substrate") {
        true => choose_saved(backend)?,
        false => None,
    };
    let selected_saved = saved.is_some();
    if let Some(saved) = saved {
        answers = saved;
    } else if matches!(backend, "gke" | "gce") {
        let kind = if backend == "gke" {
            CloudKind::Gke
        } else {
            CloudKind::Gce
        };
        answers = crate::cloud_setup::guided(env, kind, &std::env::current_dir()?, args.yes)?;
    } else if backend == "remote" {
        answers.insert("remote".into(), "server".into());
    } else {
        if backend == "microsandbox" && !facts.build.microsandbox {
            return Err(Report::new("tool_unavailable", "this by build does not include Microsandbox")
                .next("install the Linux runtime package, or choose a GKE/GCE worker in by setup --guided")
                .into());
        }
        answers.insert("provider".into(), backend.into());
        if backend == "microsandbox" {
            answers
                .entry("microsandbox.image".into())
                .or_insert_with(|| {
                    format!(
                        "ghcr.io/vamsiramakrishnan/branchyard-agy:{}",
                        env!("CARGO_PKG_VERSION")
                    )
                    .into()
                });
        }
    }
    if facts.installed().any(|h| h.harness == "antigravity") || backend != "local" {
        answers
            .entry("harness".into())
            .or_insert_with(|| "antigravity".into());
    }
    if interactive {
        let mut ids = vec!["check"];
        if !selected_saved {
            match backend {
                "local" => ids.push("isolated"),
                "microsandbox" => ids.extend(["microsandbox.image", "secrets"]),
                "remote" => ids.extend(["remote.url", "remote.token_file", "remote.ca_file"]),
                "substrate" => ids.extend([
                    "substrate.endpoint",
                    "substrate.router",
                    "substrate.atespace",
                    "substrate.template",
                    "substrate.key",
                    "substrate.api_token_file",
                    "substrate.ca",
                    "substrate.server_name",
                    "substrate.egress",
                    "secrets",
                ]),
                _ => {}
            }
        }
        if !answers.contains_key("harness") {
            ids.insert(0, "harness");
        }
        for id in ids {
            if id == "check" && facts.suggested_check.is_some() {
                continue;
            }
            let questions = branchyard_setup::questions(Topic::Project, facts, &answers);
            if let Some(question) = questions.iter().find(|q| q.id == id) {
                let value = crate::wizard::ask(question).map_err(crate::wizard::cancelled)?;
                answers.insert(id.into(), value);
            }
        }
        let questions = branchyard_setup::questions(Topic::Project, facts, &answers);
        for question in questions.iter().filter(|q| q.id.starts_with("secret.")) {
            let value = crate::wizard::ask(question).map_err(crate::wizard::cancelled)?;
            answers.insert(question.id.clone(), value);
        }
    }
    Ok(answers)
}

/// On a terminal, offer the deployments guided setup provisioned before
/// (`backend` `substrate` lists only Substrate ones), and the answers of the
/// one chosen; `None` to enter connection details instead.
fn choose_saved(backend: &str) -> Result<Option<BTreeMap<String, Value>>, Failure> {
    let saved: Vec<_> = crate::cloud_setup::discover_saved_environments()
        .into_iter()
        .filter(|saved| {
            backend == "remote" || saved.kind == crate::cloud_setup::SavedKind::Substrate
        })
        .collect();
    let Some(first) = saved.first() else {
        return Ok(None);
    };
    let choice = {
        let mut prompt = cliclack::select("Select an existing remote environment")
            .initial_value(first.id.as_str());
        for environment in &saved {
            prompt = prompt.item(
                environment.id.as_str(),
                &environment.label,
                &environment.hint,
            );
        }
        prompt = prompt.item(
            "manual",
            "Enter connection details manually",
            "provide a server URL, SSH target or Substrate endpoint",
        );
        prompt
            .interact()
            .map_err(crate::wizard::cancelled)?
            .to_owned()
    };
    Ok(saved
        .into_iter()
        .find(|environment| environment.id == choice)
        .map(|environment| environment.answers))
}
