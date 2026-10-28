//! Guided cloud choices backed by the packaged provisioning helpers.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::commands::{Env, Failure};
use crate::errors::Report;

/// Cloud destination offered by the setup interview.
#[derive(Clone, Copy)]
pub(crate) enum CloudKind {
    Gke,
    Gce,
}

fn input(label: &str, initial: &str) -> Result<String, Failure> {
    cliclack::input(label)
        .default_input(initial)
        .validate(|value: &String| {
            if value.trim().is_empty() || value.contains(['\n', '\r', '\0']) {
                Err("Enter a single nonempty value")
            } else {
                Ok(())
            }
        })
        .interact()
        .map_err(crate::wizard::cancelled)
}

/// Export this release's exact helpers into an atomic, versioned cache.
fn resources() -> Result<PathBuf, Failure> {
    let version = crate::setup_io::first_line(
        Path::new("python3"),
        &[
            "-c",
            "import sys; print('ok' if sys.version_info >= (3, 10) else 'old')",
        ],
        None,
        std::time::Duration::from_secs(5),
    );
    if version.as_deref() != Some("ok") {
        return Err(
            Report::new("tool_unavailable", "cloud setup needs Python 3.10 or newer")
                .next("install Python 3.10+ as python3, then rerun by setup --guided")
                .into(),
        );
    }
    let cache = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .ok_or_else(|| {
            Report::new("config", "cannot locate your private state directory")
                .next("set HOME, then rerun by setup --guided")
        })?
        .join("branchyard/onboarding")
        .join(env!("CARGO_PKG_VERSION"));
    fs::create_dir_all(&cache)?;
    let resources = [
        (
            "cloud_setup.py",
            include_str!("../../../tools/cloud_setup.py"),
        ),
        (
            "substrate_gke.py",
            include_str!("../../../tools/substrate_gke.py"),
        ),
        (
            "compute_gce.py",
            include_str!("../../../tools/compute_gce.py"),
        ),
    ];
    for (name, contents) in resources {
        let id = branchyard_support::new_ulid().map_err(|error| {
            Report::new("io", error.to_string()).next("rerun by setup --guided")
        })?;
        let staged = cache.join(format!(".{name}-{id}.tmp"));
        fs::write(&staged, contents)?;
        fs::rename(staged, cache.join(name))?;
    }
    Ok(cache)
}

/// Find only a private deployment created by the guided GKE workflow.
fn managed_state(token: Option<&str>, prefix: &Path) -> Result<Option<PathBuf>, Failure> {
    use std::os::unix::fs::MetadataExt;

    let Some(state) = token.and_then(|token| Path::new(token).parent()) else {
        return Ok(None);
    };
    if !state.starts_with(prefix) || !state.is_dir() {
        return Ok(None);
    }
    let state = fs::canonicalize(state)?;
    if !state.starts_with(fs::canonicalize(prefix)?) {
        return Ok(None);
    }
    let uid = rustix::process::getuid().as_raw();
    let metadata = fs::metadata(&state)?;
    if metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(Report::new(
            "config",
            "saved cloud connection state is not private and owned by you",
        )
        .next("inspect its ownership and permissions, then rerun by setup --guided")
        .into());
    }
    for name in ["deployment.json", "project-answers.json"] {
        let path = state.join(name);
        if !path.is_file() {
            return Ok(None);
        }
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_file() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
            return Err(Report::new(
                "config",
                "saved cloud deployment records are not private regular files",
            )
            .next("inspect the deployment ownership and permissions, then rerun by setup --guided")
            .into());
        }
    }
    Ok(Some(state))
}

/// Refresh authentication and local forwards for an owned saved GKE connection.
pub(crate) fn refresh_saved(directory: &Path) -> Result<(), Failure> {
    let Some(effective) = branchyard::configure::configuration(directory)? else {
        return Ok(());
    };
    let Some(saved) = effective.config.substrate.as_ref() else {
        return Ok(());
    };
    let prefix = dirs::home_dir()
        .ok_or_else(|| {
            Report::new("config", "cannot locate your home directory")
                .next("set HOME, then rerun by setup --guided")
        })?
        .join(".local/state/branchyard/cloud");
    let Some(state) = managed_state(saved.api_token_file.as_deref(), &prefix)? else {
        return Ok(());
    };
    let cache = resources()?;
    let result = Command::new("python3")
        .arg(cache.join("cloud_setup.py"))
        .arg("--refresh-state")
        .arg(&state)
        .arg("--by")
        .arg(std::env::current_exe()?)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()?;
    if !result.status.success() {
        return Err(Report::new(
            "command_failed",
            "could not refresh the saved GKE connection",
        )
        .next("follow the reported remedy and rerun by setup --guided")
        .into());
    }
    let answers: BTreeMap<String, Value> =
        serde_json::from_slice(&result.stdout).map_err(|error| {
            Report::new(
                "remote_protocol",
                format!("could not read the refreshed connection: {error}"),
            )
            .next("rerun by setup --guided")
        })?;
    let source = effective.sources.get("substrate.endpoint");
    let path = match source {
        Some(
            branchyard_setup::config::Source::Project { path }
            | branchyard_setup::config::Source::User { path },
        ) => PathBuf::from(path),
        _ => {
            return Err(Report::new(
                "config",
                "saved Substrate connection has no editable source file",
            )
            .next("by config path")
            .into())
        }
    };
    let previous = fs::read_to_string(&path)?;
    if let Some(updated) = updated_connection(&previous, &answers)? {
        use std::os::unix::fs::PermissionsExt;
        if fs::symlink_metadata(&path)?.file_type().is_symlink() {
            return Err(Report::new("config", "saved connection uses a symlink")
                .next("update its endpoint/router in the source file shown by by config path")
                .into());
        }
        let mode = fs::metadata(&path)?.permissions().mode() & 0o777;
        crate::setup_io::write_file(&path, &updated, mode)?;
    }
    Ok(())
}

/// Preserve project comments and unrelated settings when a forward's port changes.
fn updated_connection(
    previous: &str,
    answers: &BTreeMap<String, Value>,
) -> Result<Option<String>, Failure> {
    let mut document: toml_edit::DocumentMut = previous.parse().map_err(|error| {
        Report::new(
            "config",
            format!("could not read saved Substrate configuration: {error}"),
        )
        .next("by config path")
    })?;
    let mut changed = false;
    for key in ["endpoint", "router"] {
        let Some(value) = answers
            .get(&format!("substrate.{key}"))
            .and_then(Value::as_str)
        else {
            return Err(Report::new(
                "remote_protocol",
                "refreshed cloud connection omitted its endpoint or router",
            )
            .next("rerun by setup --guided")
            .into());
        };
        if document["substrate"][key].as_str() != Some(value) {
            document["substrate"][key] = toml_edit::value(value);
            changed = true;
        }
    }
    let updated = document.to_string();
    branchyard_setup::config::parse(&updated).map_err(|error| {
        Report::new(
            "config",
            format!("refreshed Substrate configuration would not load: {error}"),
        )
        .next("by config path")
    })?;
    Ok(changed.then_some(updated))
}

/// Review a dedicated cloud deployment, apply it, and return project answers.
pub(crate) fn guided(
    env: &Env,
    kind: CloudKind,
    directory: &Path,
    yes: bool,
) -> Result<BTreeMap<String, Value>, Failure> {
    if yes || !env.stdin_tty || !env.stdout_tty {
        return Err(Report::new(
            "manual",
            "cloud setup needs a terminal to review the project and resources",
        )
        .next("by setup --guided")
        .into());
    }
    let project = input("Google Cloud project", "")?;
    let zone = input("Zone", "us-central1-a")?;
    let name = input(
        "Dedicated deployment name (reuse it to resume)",
        "branchyard-dev",
    )?;
    let account_prompt = match kind {
        CloudKind::Gke => "Node service account email (leave blank to create a dedicated account)",
        CloudKind::Gce => "Worker service account email (leave blank for no cloud API access)",
    };
    let service_account: String = cliclack::input(account_prompt)
        .required(false)
        .interact()
        .map_err(crate::wizard::cancelled)?;
    let kind_name = match kind {
        CloudKind::Gke => "gke",
        CloudKind::Gce => "gce",
    };
    let runtime: &str = match kind {
        CloudKind::Gke => cliclack::select("Actor isolation")
            .item("gvisor", "gVisor", "Kernel sandbox")
            .item("microvm", "MicroVM", "Nested virtualization")
            .interact()
            .map_err(crate::wizard::cancelled)?,
        CloudKind::Gce => cliclack::select("Worker execution")
            .item("local", "Private worker", "Processes on a dedicated VM")
            .item(
                "microsandbox",
                "Microsandbox",
                "KVM guests; hard memory ceiling remains unqualified",
            )
            .interact()
            .map_err(crate::wizard::cancelled)?,
    };
    let cache = resources()?;
    let mut command = Command::new("python3");
    command
        .arg(cache.join("cloud_setup.py"))
        .args([
            "--kind",
            kind_name,
            "--project",
            &project,
            "--zone",
            &zone,
            "--name",
            &name,
        ])
        .args([
            "--service-account",
            &service_account,
            "--runtime",
            runtime,
            "--repo",
        ])
        .arg(directory)
        .args(["--version", env!("CARGO_PKG_VERSION")])
        .arg("--by")
        .arg(std::env::current_exe()?)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit());
    if matches!(kind, CloudKind::Gce) {
        if let Some(name) = ["GEMINI_API_KEY", "GOOGLE_API_KEY"]
            .into_iter()
            .find(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
        {
            let provision = cliclack::confirm(format!(
                "Provision {name} into the worker's private credential store?"
            ))
            .initial_value(false)
            .interact()
            .map_err(crate::wizard::cancelled)?;
            if provision {
                command.args(["--forward-secret", name]);
            }
        }
    }
    let planned = command.output()?;
    if !planned.status.success() {
        return Err(Report::new("manual", "cloud prerequisites are not ready")
            .next("follow the remedy above, then rerun by setup --guided")
            .into());
    }
    let plan: Value = serde_json::from_slice(&planned.stdout).map_err(|error| {
        Report::new(
            "remote_protocol",
            format!("could not read the cloud plan: {error}"),
        )
        .next("rerun by setup --guided")
    })?;
    cliclack::note("Cloud resources", crate::json::text(&plan))?;
    let approved = cliclack::confirm("Create or resume these billable cloud resources?")
        .initial_value(false)
        .interact()
        .map_err(crate::wizard::cancelled)?;
    if !approved {
        return Err(Report::new(
            "manual",
            "cloud setup cancelled; no cloud resources were changed",
        )
        .next("by setup --guided")
        .into());
    }
    command.arg("--apply");
    let applied = command.output()?;
    if !applied.status.success() {
        return Err(Report::new(
            "command_failed",
            "cloud setup stopped; the private deployment record names completed steps",
        )
        .next("rerun by setup --guided with the same deployment name to resume")
        .into());
    }
    serde_json::from_slice(&applied.stdout).map_err(|error| {
        Report::new(
            "remote_protocol",
            format!("could not read the cloud configuration: {error}"),
        )
        .next("rerun by setup --guided")
        .into()
    })
}

#[allow(clippy::unwrap_used)] // tests: setup failures are test failures
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unattended_cloud_setup_never_creates_resources() {
        let env = Env {
            stdin_tty: false,
            stdout_tty: false,
            stderr_tty: false,
            color: false,
            notify: crate::notify::Settings::default(),
        };
        for kind in [CloudKind::Gce, CloudKind::Gke] {
            assert!(
                matches!(guided(&env, kind, Path::new("."), true), Err(Failure::Report(report)) if report.kind == "manual" && report.message.contains("needs a terminal") && report.exit() == crate::errors::Exit::Environment)
            );
        }
    }

    #[test]
    fn only_owned_private_connection_state_is_refreshable() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = branchyard_testkit::Scratch::new("cloud-refresh");
        let prefix = scratch.path().join("cloud");
        let state = prefix.join("gke/project/zone/name");
        fs::create_dir_all(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let token = state.join("control.token");
        assert_eq!(managed_state(None, &prefix).unwrap(), None);
        assert_eq!(
            managed_state(Some("/unmanaged/control.token"), &prefix).unwrap(),
            None
        );
        assert_eq!(managed_state(token.to_str(), &prefix).unwrap(), None);
        for name in ["deployment.json", "project-answers.json"] {
            fs::write(state.join(name), "{}").unwrap();
            fs::set_permissions(state.join(name), fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert_eq!(
            managed_state(token.to_str(), &prefix).unwrap(),
            Some(state.clone())
        );
        fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(managed_state(token.to_str(), &prefix).is_err());
    }

    #[test]
    fn refreshed_ports_preserve_config_comments_and_other_fields() {
        let previous = "# project comment\n[defaults]\nharness = \"antigravity\"\n[substrate]\nendpoint = \"https://127.0.0.1:10001\"\nrouter = \"http://127.0.0.1:10002/\"\ntemplate = \"agy\"\nkey = \"bridge.key\"\negress = { mode = \"deny\" }\n";
        let answers = BTreeMap::from([
            (
                "substrate.endpoint".into(),
                Value::from("https://127.0.0.1:20001"),
            ),
            (
                "substrate.router".into(),
                Value::from("http://127.0.0.1:20002/"),
            ),
        ]);
        let updated = updated_connection(previous, &answers).unwrap().unwrap();
        assert!(updated.starts_with("# project comment"));
        assert!(updated.contains("harness = \"antigravity\""));
        assert!(updated.contains("key = \"bridge.key\""));
        assert!(updated.contains(":20001"));
        assert!(updated_connection(&updated, &answers).unwrap().is_none());
        assert!(updated_connection(previous, &BTreeMap::new()).is_err());
    }
}
