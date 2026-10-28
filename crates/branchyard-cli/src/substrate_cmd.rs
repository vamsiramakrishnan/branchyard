//! Prepare the saved Substrate template and explicitly probe a disposable actor.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use branchyard_bridge::Signer;
use branchyard_sandbox::{ExecSpec, SandboxProvider, SandboxSpec};
use branchyard_substrate::template::{BridgeOptions, BridgeTemplate, TemplateResources};
use branchyard_substrate::{pb, SetupAction, SetupEvent, SetupPlan, SubstrateProvider};
use clap::{Args, Subcommand, ValueEnum};
use serde::Serialize;

use crate::args::Globals;
use crate::commands::{self, Env, Failure, Outcome};

#[derive(Clone, Debug, PartialEq, Subcommand)]
/// Prepare or inspect the configured Agent Substrate resources.
pub enum Action {
    /// Create or verify the atespace and immutable bridge template in saved configuration
    Setup(Setup),
    /// Authenticate and inspect the template; optionally execute a disposable actor
    Check {
        /// Prove bridge execution and file transfer, then delete the actor; calls no model
        #[arg(long)]
        smoke: bool,
        /// Also run this harness executable with --version inside the smoke actor
        #[arg(long, requires = "smoke", value_name = "PROGRAM")]
        harness: Option<String>,
        /// Print a machine-readable report
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Args)]
/// Image and runtime settings for a saved Substrate template.
pub struct Setup {
    /// OCI image containing the bridge, git and harness, pinned with @sha256:...
    #[arg(long)]
    image: String,
    /// Cluster SandboxConfig to use
    #[arg(long, default_value = "gvisor-default")]
    sandbox_config: String,
    /// Snapshot storage URI supported by this Substrate deployment
    #[arg(long)]
    storage: String,
    /// Runtime class of the SandboxConfig
    #[arg(long, value_enum, default_value = "gvisor")]
    runtime: Runtime,
    /// Bridge executable in the image
    #[arg(long, default_value = "/usr/local/bin/branchyard-bridge")]
    bridge: String,
    /// Unprivileged UID:GID for harness execution and transferred files
    #[arg(long, default_value = "1000:1000", value_parser = run_as)]
    run_as: (u32, u32),
    /// CPU quantity reserved for each actor
    #[arg(long, default_value = "2")]
    cpu: String,
    /// Memory quantity reserved for each actor
    #[arg(long, default_value = "4Gi")]
    memory: String,
    /// WorkerPool label selector; repeat for each KEY=VALUE
    #[arg(long, value_name = "KEY=VALUE", value_parser = label)]
    worker_label: Vec<(String, String)>,
    /// Inspect and print proposed changes without creating a key or remote resources
    #[arg(long)]
    plan: bool,
    /// Apply the displayed setup without an interactive confirmation
    #[arg(long)]
    yes: bool,
    /// Print a machine-readable report
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Debug, PartialEq, ValueEnum)]
enum Runtime {
    Gvisor,
    Microvm,
}

fn run_as(value: &str) -> Result<(u32, u32), String> {
    let (uid, gid) = value.split_once(':').ok_or("expected UID:GID")?;
    let uid = uid.parse().map_err(|_| "UID must be an unsigned integer")?;
    let gid = gid.parse().map_err(|_| "GID must be an unsigned integer")?;
    if uid == 0 || gid == 0 {
        return Err("use an unprivileged UID and GID for harness execution".into());
    }
    Ok((uid, gid))
}

fn label(value: &str) -> Result<(String, String), String> {
    let (key, value) = value.split_once('=').ok_or("expected KEY=VALUE")?;
    if key.is_empty() || value.is_empty() {
        return Err("worker label key and value must be nonempty".into());
    }
    Ok((key.into(), value.into()))
}

fn failure(error: impl std::fmt::Display) -> Failure {
    Failure::Message(error.to_string())
}

fn report(value: &impl Serialize, as_json: bool, text: &str) -> Outcome {
    if as_json {
        commands::print(&(serde_json::to_string_pretty(value).map_err(failure)? + "\n"))
    } else {
        commands::print(&(text.to_owned() + "\n"))
    }
}

/// Run Substrate setup or inspection from the saved repository configuration.
pub fn main(env: &Env, globals: &Globals, action: &Action) -> Outcome {
    if globals.remote.is_some() || crate::workspace_cmd::in_harness().is_some() {
        return Err(failure(
            "run by substrate in the configured repository, outside a harness or --remote session",
        ));
    }
    let config = branchyard::configure::configuration(&std::env::current_dir()?)?
        .ok_or_else(|| failure("save [substrate] in branchyard.toml first; run by init project"))?;
    let config = &config.config;
    let saved = config
        .substrate
        .as_ref()
        .ok_or_else(|| failure("missing [substrate] configuration; run by init project"))?;
    let options = branchyard::configure::substrate_options(saved);
    match action {
        Action::Setup(args) => setup(env, &options, args),
        Action::Check {
            smoke,
            harness,
            json,
        } => {
            let provider =
                SubstrateProvider::connect(options.connection_config()?).map_err(failure)?;
            let progress = (env.stderr_tty && !json).then(cliclack::spinner);
            if let Some(progress) = &progress {
                progress.start("Authenticating and inspecting the bridge template");
            }
            let inspection = provider.inspect_template();
            if let Some(progress) = &progress {
                if inspection.is_ok() {
                    progress.stop("Control and bridge template verified");
                } else {
                    progress.error("Template check stopped");
                }
            }
            let inspection = inspection.map_err(failure)?;
            let version = if *smoke {
                probe(
                    &provider,
                    options.workdir(),
                    harness.as_deref(),
                    env.stderr_tty && !json,
                )?
            } else {
                None
            };
            let text = if *smoke {
                let mut text = "Substrate ready: authenticated control, bridge execution, file round trip and actor cleanup passed.".to_owned();
                if let Some(version) = &version {
                    text.push_str("\nHarness: ");
                    text.push_str(
                        version
                            .lines()
                            .next()
                            .filter(|line| !line.is_empty())
                            .unwrap_or("--version succeeded without output"),
                    );
                }
                text.push_str("\nModel authentication not checked.");
                text
            } else {
                format!("Control authenticated; template {}/{} has a valid bridge port, identity and matching signing key.\nNext: by substrate check --smoke --harness agy. Model authentication not checked.", inspection.atespace, inspection.name)
            };
            report(
                &serde_json::json!({"control": "authenticated", "template": "validated", "template_details": {"atespace": inspection.atespace, "name": inspection.name, "image": inspection.image, "bridge_port": inspection.bridge_port, "signer": "matches"}, "smoke": *smoke, "harness_version": version, "model": "not_checked"}),
                *json,
                &text,
            )
        }
    }
}

fn setup(env: &Env, options: &branchyard::SubstrateOptions, args: &Setup) -> Outcome {
    for (flag, value) in [("--cpu", &args.cpu), ("--memory", &args.memory)] {
        if value.trim().is_empty() {
            return Err(failure(format!("{flag} must not be empty")));
        }
    }
    let mut labels = BTreeMap::new();
    for (key, value) in &args.worker_label {
        if labels.insert(key.clone(), value.clone()).is_some() {
            return Err(failure(format!(
                "worker label {key:?} was provided more than once"
            )));
        }
    }
    let may_apply = !args.plan && (args.yes || (env.stdin_tty && env.stderr_tty && !args.json));
    let (signer, document) = match Signer::read(&options.key) {
        Ok(signer) => (signer, None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let (signer, document) = Signer::generate()?;
            (signer, Some(document))
        }
        Err(error) => return Err(error.into()),
    };
    let public_key = signer.public_key();
    let provider = SubstrateProvider::connect(options.connection_config_with_signer(signer)?)
        .map_err(failure)?;
    let template = BridgeTemplate {
        name: options
            .template
            .rsplit('/')
            .next()
            .unwrap_or(&options.template)
            .into(),
        image: args.image.clone(),
        bridge: args.bridge.clone(),
        public_key: public_key.clone(),
        sandbox_class: match args.runtime {
            Runtime::Gvisor => pb::SandboxClass::Gvisor,
            Runtime::Microvm => pb::SandboxClass::Microvm,
        },
        sandbox_config: args.sandbox_config.clone(),
        storage_location: args.storage.clone(),
    };
    let bridge = BridgeOptions {
        run_as: Some(args.run_as),
        worker_selector: labels,
        resources: TemplateResources {
            cpu: Some(args.cpu.clone()),
            memory: Some(args.memory.clone()),
        },
        ..Default::default()
    };
    let planning = (env.stderr_tty && !args.json).then(cliclack::spinner);
    if let Some(progress) = &planning {
        progress.start("Authenticating and comparing the saved template");
    }
    let plan = provider.plan_setup(&template, &bridge);
    if let Some(progress) = &planning {
        if plan.is_ok() {
            progress.stop("Setup plan verified");
        } else {
            progress.error("Setup stopped before writing changes");
        }
    }
    let plan = plan.map_err(failure)?;
    let summary = describe_plan(options, args, &plan, document.is_some());
    if !may_apply {
        return report(
            &serde_json::json!({
                "status": "planned", "key": {"path": options.key, "action": if document.is_some() { "create" } else { "reuse" }},
                "plan": plan,
            }),
            args.json,
            &format!("{summary}\nApply with the same command and --yes."),
        );
    }
    if !args.yes {
        cliclack::note("Substrate setup", &summary)?;
        let approved = cliclack::confirm("Apply these changes?")
            .initial_value(false)
            .interact()
            .map_err(crate::wizard::cancelled)?;
        if !approved {
            return Err(failure("setup cancelled; no changes applied"));
        }
    }
    if let Some(document) = document {
        if let Some(parent) = options.key.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut key = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600)
            .open(&options.key).map_err(|error| failure(format!("could not create bridge key {}: {error}; no remote changes applied; rerun setup to recheck the key", options.key.display())))?;
        key.write_all(&document)?;
        key.sync_all()?;
    }
    if Signer::read(&options.key)?.public_key() != public_key {
        return Err(failure(
            "bridge key changed after planning; no remote changes applied; rerun setup",
        ));
    }
    let progress = (env.stderr_tty && !args.json).then(cliclack::spinner);
    if let Some(progress) = &progress {
        progress.start("Preparing Substrate template");
    }
    let result = provider.apply_setup(&plan, |event| {
        if let Some(progress) = &progress {
            progress.set_message(match event {
                SetupEvent::Checking => "Rechecking the setup plan".into(),
                SetupEvent::CreatingAtespace(name) => format!("Creating atespace {name}"),
                SetupEvent::CreatingTemplate => "Creating the immutable bridge template".into(),
                SetupEvent::Ready => "Template verified".into(),
            });
        }
    });
    if let Some(progress) = &progress {
        if result.is_ok() {
            progress.stop("Substrate template ready");
        } else {
            progress.error("Setup stopped; fix the reported issue and rerun the same command");
        }
    }
    let result = result.map_err(failure)?;
    report(
        &result,
        args.json,
        "Substrate template ready. Next: by substrate check --smoke --harness agy.",
    )
}

fn describe_plan(
    options: &branchyard::SubstrateOptions,
    args: &Setup,
    plan: &SetupPlan,
    new_key: bool,
) -> String {
    let mut lines = vec![
        format!("Control: {}", options.endpoint),
        format!("Template: {}", plan.template),
        format!("Image: {}", plan.image),
        format!(
            "Resources: {} CPU, {} memory; harness identity {}:{}",
            args.cpu, args.memory, args.run_as.0, args.run_as.1
        ),
        format!(
            "{} bridge key: {}",
            if new_key { "Create private" } else { "Reuse" },
            options.key.display()
        ),
    ];
    for action in &plan.actions {
        lines.push(match action {
            SetupAction::CreateAtespace { name } => format!("Create atespace: {name}"),
            SetupAction::CreateTemplate => "Create immutable bridge template".into(),
        });
    }
    if plan.actions.is_empty() {
        lines.push("Template already matches; no remote changes needed".into());
    }
    lines.join("\n")
}

struct Cancellation {
    requested: Arc<AtomicBool>,
    handlers: Vec<signal_hook::SigId>,
}

impl Cancellation {
    fn register() -> std::io::Result<Self> {
        let mut cancellation = Self {
            requested: Arc::new(AtomicBool::new(false)),
            handlers: Vec::new(),
        };
        for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
            cancellation.handlers.push(signal_hook::flag::register(
                signal,
                cancellation.requested.clone(),
            )?);
        }
        Ok(cancellation)
    }

    fn check(&self) -> Result<(), Failure> {
        if self.requested.load(Ordering::Relaxed) {
            Err(failure("Substrate smoke cancelled"))
        } else {
            Ok(())
        }
    }
}

impl Drop for Cancellation {
    fn drop(&mut self) {
        for handler in self.handlers.drain(..) {
            signal_hook::low_level::unregister(handler);
        }
    }
}

fn probe(
    provider: &SubstrateProvider,
    workdir: &str,
    harness: Option<&str>,
    interactive: bool,
) -> Result<Option<String>, Failure> {
    let cancellation = Cancellation::register()?;
    let progress = interactive.then(cliclack::spinner);
    if let Some(progress) = &progress {
        progress.start("Starting a disposable Substrate actor");
    }
    let name = format!(
        "by-check-{}",
        branchyard_support::new_ulid()
            .map_err(failure)?
            .to_ascii_lowercase()
    );
    let result = (|| {
        cancellation.check()?;
        provider.ensure(&SandboxSpec::new(&name)).map_err(failure)?;
        cancellation.check()?;
        if let Some(progress) = &progress {
            progress.set_message("Checking bridge file transfer");
        }
        let endpoint = provider.endpoint(&name).map_err(failure)?;
        let path = Path::new(workdir).join(".branchyard-probe");
        let content = b"branchyard bridge round trip\n";
        endpoint
            .put_file(&path, 0o600, &mut content.as_slice())
            .map_err(|error| {
                failure(format!("write bridge probe to {}: {error}", path.display()))
            })?;
        let mut received = Vec::new();
        endpoint.get_file(&path, &mut received).map_err(|error| {
            failure(format!(
                "read bridge probe from {}: {error}",
                path.display()
            ))
        })?;
        if received != content {
            return Err(failure("bridge returned different file contents"));
        }
        cancellation.check()?;
        if let Some(progress) = &progress {
            progress.set_message("Checking executable inside the actor");
        }
        let argv = harness.map_or_else(
            || vec!["true".into()],
            |program| vec![program.to_owned(), "--version".into()],
        );
        let command = argv.join(" ");
        let mut process = provider
            .exec(
                &name,
                &ExecSpec {
                    argv,
                    cwd: workdir.into(),
                    env: Default::default(),
                },
            )
            .map_err(|error| failure(format!("execute {command} in the actor: {error}")))?;
        drop(process.take_stdin());
        let output = probe_output(process.as_mut(), &cancellation, Duration::from_secs(30))
            .map_err(|error| failure(format!("probe {command} in the actor: {error}")))?;
        Ok(harness.map(|_| output))
    })();
    if let Some(progress) = &progress {
        progress.set_message("Removing the disposable actor");
    }
    let cleanup = provider.destroy(&name).map_err(failure);
    if let Some(progress) = &progress {
        if result.is_ok() && cleanup.is_ok() {
            progress.stop("Smoke passed; disposable actor removed");
        } else {
            progress.error("Smoke stopped");
        }
    }
    match (result, cleanup) {
        (Ok(result), Ok(())) => Ok(result),
        (Err(error), Ok(())) => Err(failure(format!("{error}; disposable actor {name} removed"))),
        (Ok(_), Err(cleanup)) => Err(failure(format!("smoke passed but actor {name} cleanup failed: {cleanup}; remove the actor before retrying"))),
        (Err(error), Err(cleanup)) => Err(failure(format!("{error}; actor {name} cleanup also failed: {cleanup}; remove the actor before retrying"))),
    }
}

fn probe_output(
    process: &mut dyn branchyard_sandbox::Process,
    cancellation: &Cancellation,
    timeout: Duration,
) -> Result<String, Failure> {
    const LIMIT: u64 = 64 * 1024;
    let (send, receive) = mpsc::channel();
    let mut streams = [None, None];
    for (index, pipe) in [process.take_stdout(), process.take_stderr()]
        .into_iter()
        .enumerate()
    {
        if let Some(pipe) = pipe {
            let send = send.clone();
            std::thread::Builder::new()
                .name("substrate-smoke-output".into())
                .spawn(move || {
                    let mut bytes = Vec::new();
                    let result = pipe.take(LIMIT + 1).read_to_end(&mut bytes).map(|_| bytes);
                    // Cancellation closes the receiver while actor destruction closes the pipes.
                    branchyard_support::best_effort(
                        "deliver smoke output",
                        send.send((index, result)),
                    );
                })?;
        } else {
            streams[index] = Some(Vec::new());
        }
    }
    let started = std::time::Instant::now();
    loop {
        cancellation.check()?;
        if started.elapsed() >= timeout {
            return Err(failure(format!(
                "smoke executable did not finish within {} seconds",
                timeout.as_secs()
            )));
        }
        let status = process.try_wait()?;
        if let (Some(status), [Some(stdout), Some(stderr)]) = (status, &streams) {
            let display = |bytes: &[u8]| {
                let mut text = String::from_utf8_lossy(&bytes[..bytes.len().min(LIMIT as usize)])
                    .trim()
                    .to_owned();
                if bytes.len() > LIMIT as usize {
                    text.push_str("\n[output truncated at 64 KiB]");
                }
                text
            };
            if !status.success() {
                let diagnostic = if stderr.is_empty() { stdout } else { stderr };
                return Err(failure(format!("actor executable exited with {status}; verify the image and its harness path\n{}", display(diagnostic))));
            }
            let output = if stdout.is_empty() { stderr } else { stdout };
            return Ok(display(output));
        }
        match receive.recv_timeout(Duration::from_millis(100)) {
            Ok((index, result)) => streams[index] = Some(result?),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(failure("smoke output readers stopped unexpectedly"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct WaitingProcess;

    impl branchyard_sandbox::Process for WaitingProcess {
        fn id(&self) -> String {
            "waiting".into()
        }
        fn take_stdin(&mut self) -> Option<Box<dyn Write + Send>> {
            None
        }
        fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
            None
        }
        fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>> {
            None
        }
        fn try_wait(&mut self) -> std::io::Result<Option<branchyard_sandbox::ExitStatus>> {
            Ok(None)
        }
        fn wait(&mut self) -> std::io::Result<branchyard_sandbox::ExitStatus> {
            Err(std::io::Error::other("must poll, not block"))
        }
        fn kill(&mut self) -> std::io::Result<()> {
            Ok(())
        }
        fn teardown(&mut self) -> Vec<String> {
            Vec::new()
        }
    }

    #[test]
    fn smoke_output_wait_is_bounded_and_cancellable() {
        let cancellation = Cancellation {
            requested: Arc::new(AtomicBool::new(false)),
            handlers: Vec::new(),
        };
        let timeout = probe_output(&mut WaitingProcess, &cancellation, Duration::ZERO);
        assert!(matches!(timeout, Err(error) if error.to_string().contains("did not finish")));
        cancellation.requested.store(true, Ordering::Relaxed);
        let cancelled = probe_output(&mut WaitingProcess, &cancellation, Duration::from_secs(30));
        assert!(matches!(cancelled, Err(error) if error.to_string().contains("cancelled")));
    }

    #[test]
    fn setup_identity_and_labels_are_validated_at_the_boundary() {
        assert_eq!(run_as("1000:1000"), Ok((1000, 1000)));
        for invalid in ["0:1000", "1000:0", "-1:1000", "user", "1:x"] {
            assert!(run_as(invalid).is_err());
        }
        assert_eq!(
            label("workload=branchyard"),
            Ok(("workload".into(), "branchyard".into()))
        );
        for invalid in ["workload", "=branchyard", "workload="] {
            assert!(label(invalid).is_err());
        }
    }
}
