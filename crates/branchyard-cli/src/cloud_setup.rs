//! Guided cloud choices backed by the packaged provisioning helpers.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::args::Globals;
use crate::commands::{self, Env, Failure, Outcome};
use crate::errors::Report;

/// Review or provision a dedicated cloud worker from the operator's machine.
#[derive(Clone, Debug, PartialEq, clap::Subcommand)]
pub enum Action {
    /// Prepare a dedicated GCE worker for native SSH; prints an offline JSON plan by default
    #[command(
        after_help = "Examples:\n  by compute gce --project my-project --zone us-central1-a --name by-worker --directory . --tunnel-through-iap\n  # Review the JSON plan, then repeat with --apply.\n\nIAP opens TCP 22 only from 35.235.240.0/20 to this deployment's VM tag.\nIt requires your Google Cloud IAP tunnel permission. Add --internal-ip to omit\nthe VM's external address; package downloads then require existing outbound access.\nCalls no model. Resources remain billable until you delete them. See docs/compute-gce.md."
    )]
    Gce(Gce),
}

/// Settings forwarded to the release's packaged GCE provisioning helper.
#[derive(Clone, Debug, PartialEq, clap::Args)]
pub struct Gce {
    /// Google Cloud project with billing and the Compute Engine API enabled
    #[arg(long)]
    project: String,
    /// Zone for the dedicated worker, such as us-central1-a
    #[arg(long)]
    zone: String,
    /// Dedicated VM name; reuse the same options to resume, never adopts a foreign VM
    #[arg(long)]
    name: String,
    /// Copy this repository's clean committed HEAD; omitted creates an empty repository
    #[arg(long, value_name = "PATH")]
    directory: Option<PathBuf>,
    /// Branchyard release to install; defaults to this CLI's version
    #[arg(long, default_value = env!("CARGO_PKG_VERSION"))]
    version: String,
    /// Published worker image pinned by release tag or @sha256 digest; defaults to --version
    #[arg(long, value_name = "PIN")]
    image: Option<String>,
    /// Worker execution: local processes or Microsandbox KVM guests
    #[arg(long, value_parser = ["local", "microsandbox"], default_value = "local")]
    backend: String,
    /// VM machine type; Microsandbox needs n2-standard or c3-standard
    #[arg(long, default_value = "n2-standard-4")]
    machine_type: String,
    /// Existing service account in this project; omitted attaches no account
    #[arg(long, value_name = "EMAIL")]
    service_account: Option<String>,
    /// Existing VPC network; creates no network or NAT
    #[arg(long, default_value = "default")]
    network: String,
    /// Existing subnet for the VM
    #[arg(long, default_value = "default")]
    subnet: String,
    /// Balanced persistent disk size in GB
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(30..))]
    disk_size: u32,
    /// Omit the external IP; needs an existing private route or --tunnel-through-iap, plus outbound access
    #[arg(long)]
    internal_ip: bool,
    /// SSH through Google Cloud IAP; allow TCP 22 from 35.235.240.0/20 only to this VM's dedicated tag
    #[arg(long)]
    tunnel_through_iap: bool,
    /// Read this environment variable into the worker's private keystore over SSH stdin; never pass a key value
    #[arg(long, value_name = "ENV_NAME", value_parser = ["GEMINI_API_KEY", "GOOGLE_API_KEY"])]
    forward_secret: Option<String>,
    /// Private deployment records; default is ~/.local/state/branchyard/compute-gce/PROJECT/ZONE/NAME
    #[arg(long, value_name = "PATH")]
    state_dir: Option<PathBuf>,
    /// Create or resume the reviewed billable VM, firewall when using IAP, and owned SSH configuration
    #[arg(long)]
    apply: bool,
}

fn gce_command(args: &Gce) -> Command {
    let mut command = Command::new("python3");
    command
        .args(["-c", include_str!("../../../tools/compute_gce.py")])
        .args([
            "--project",
            &args.project,
            "--zone",
            &args.zone,
            "--name",
            &args.name,
        ])
        .args(["--version", &args.version, "--backend", &args.backend])
        .args([
            "--machine-type",
            &args.machine_type,
            "--network",
            &args.network,
            "--subnet",
            &args.subnet,
        ])
        .arg("--disk-size")
        .arg(args.disk_size.to_string())
        .stdin(Stdio::null())
        .stderr(Stdio::inherit());
    for (flag, value) in [
        ("--image", args.image.as_deref()),
        ("--service-account", args.service_account.as_deref()),
        ("--forward-secret", args.forward_secret.as_deref()),
    ] {
        if let Some(value) = value {
            command.args([flag, value]);
        }
    }
    for (flag, value) in [
        ("--repo", args.directory.as_deref()),
        ("--state-dir", args.state_dir.as_deref()),
    ] {
        if let Some(value) = value {
            command.arg(flag).arg(value);
        }
    }
    for (flag, selected) in [
        ("--internal-ip", args.internal_ip),
        ("--tunnel-through-iap", args.tunnel_through_iap),
        ("--apply", args.apply),
    ] {
        if selected {
            command.arg(flag);
        }
    }
    command
}

/// Run the packaged cloud helper locally, outside a harness session.
pub(crate) fn compute(globals: &Globals, action: &Action) -> Outcome {
    if globals.remote.is_some() {
        return Err(
            Report::usage("by compute provisions from this machine; it takes no --remote")
                .next("run by compute without --remote, and unset BRANCHYARD_REMOTE")
                .into(),
        );
    }
    if let Some(branch) = crate::workspace_cmd::in_harness() {
        return Err(Report::new(
            "denied",
            format!("a harness running on branch {branch} cannot provision cloud resources"),
        )
        .next("run by compute in your own terminal, outside any branch")
        .into());
    }
    let Action::Gce(args) = action;
    python_ready()?;
    let output = gce_command(args).output().map_err(|_| {
        Report::new(
            "tool_unavailable",
            "could not start the packaged GCE helper",
        )
        .next("install Python 3.10+ as python3, then rerun the same by compute gce command")
    })?;
    if !output.status.success() {
        return Err(Report::new("command_failed", "GCE setup stopped; no model was called")
            .next("follow the reported remedy, inspect the private deployment state, and rerun the same command")
            .into());
    }
    commands::print(&String::from_utf8_lossy(&output.stdout))
}

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

fn python_ready() -> Result<(), Failure> {
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
                .next("install Python 3.10+ as python3, then rerun the same command")
                .into(),
        );
    }
    Ok(())
}

/// Export this release's exact helpers into an atomic, versioned cache.
fn resources() -> Result<PathBuf, Failure> {
    python_ready()?;
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
    let transport = match kind {
        CloudKind::Gce => cliclack::select("SSH connection to the worker")
            .initial_value("direct")
            .item(
                "direct",
                "External IP",
                "Requires your existing direct SSH ingress rule",
            )
            .item(
                "private",
                "Private IP",
                "Requires your private VPC route and outbound package access",
            )
            .item(
                "iap",
                "Google Cloud IAP",
                "Requires IAP IAM and permitted TCP 22 ingress; not live qualified",
            )
            .interact()
            .map_err(crate::wizard::cancelled)?,
        CloudKind::Gke => "direct",
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
    if transport == "iap" {
        command.arg("--tunnel-through-iap");
    } else if transport == "private" {
        command.arg("--internal-ip");
    }
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
    fn gce_arguments_keep_private_network_and_iap_independent() {
        let parsed = crate::args::parse_from([
            "by",
            "compute",
            "gce",
            "--project",
            "test-project",
            "--zone",
            "us-central1-a",
            "--name",
            "test-worker",
            "--directory",
            "/source with spaces",
            "--state-dir",
            "/private state",
            "--internal-ip",
            "--tunnel-through-iap",
            "--forward-secret",
            "GOOGLE_API_KEY",
            "--apply",
        ])
        .unwrap();
        let args = parsed
            .command
            .and_then(|command| match command {
                crate::args::Command::Compute {
                    action: Action::Gce(args),
                } => Some(args),
                _ => None,
            })
            .unwrap();
        let command = gce_command(&args);
        let argv = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(argv[0], "-c");
        assert_eq!(argv[1], include_str!("../../../tools/compute_gce.py"));
        for (flag, value) in [
            ("--repo", "/source with spaces"),
            ("--state-dir", "/private state"),
            ("--forward-secret", "GOOGLE_API_KEY"),
            ("--version", env!("CARGO_PKG_VERSION")),
        ] {
            assert!(argv.windows(2).any(|pair| pair == [flag, value]), "{flag}");
        }
        for flag in ["--internal-ip", "--tunnel-through-iap", "--apply"] {
            assert!(argv.iter().any(|arg| arg == flag), "{flag}");
        }
    }

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
