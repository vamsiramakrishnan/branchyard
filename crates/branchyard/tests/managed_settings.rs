//! Claude Code's managed settings through the engine, with a fake sandbox
//! provider: a sandboxed Claude Code turn writes `/etc/claude-code/
//! managed-settings.json` by an exec that runs before the harness starts,
//! carrying the branch's policy denials, bypass disabled and telemetry
//! pinned off when it is off; a write the sandbox refuses is a warning and
//! the turn goes on; a local harness gets none. The fake runs processes on
//! this host, so the write's target directory is redirected into the test's
//! scratch directory: the command is otherwise the one the engine built, and
//! its quoting is what is checked. Hermetic; nothing here is evidence about
//! a real sandbox's `/etc`.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use branchyard::{
    Activity, Branch, Policy, PolicyPreset, Provider, Provisioning, RecordedEvent, SandboxOptions,
    TaskOptions, Telemetry,
};
use branchyard_provision::ManagedWritten;
use branchyard_runtime::LocalProvider;
use branchyard_sandbox::fake::FakeProvider;
use branchyard_sandbox::{
    Capabilities, ExecSpec, Process, ProviderError, SandboxInfo, SandboxProvider, SandboxSpec,
};
use branchyard_support::LockExt as _;
use common::{fake_agent, Fixture};

/// The directory the engine writes Claude Code's managed settings into.
const MANAGED_DIR: &str = "/etc/claude-code";

/// A fake provider whose exec of the managed-settings write is intercepted:
/// the write's directory is redirected to `target`, or refused when
/// `refuse` is set (a sandbox without root, or without `/etc`). Every exec
/// is logged in order, so a test can see what ran before the harness.
struct Intercept {
    inner: Arc<FakeProvider>,
    target: PathBuf,
    refuse: bool,
    log: Mutex<Vec<Vec<String>>>,
}

impl Intercept {
    fn execs(&self) -> Vec<Vec<String>> {
        self.log.lock_recovering("exec log").clone()
    }
}

impl SandboxProvider for Intercept {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn ensure(&self, spec: &SandboxSpec) -> Result<SandboxInfo, ProviderError> {
        self.inner.ensure(spec)
    }

    fn inspect(&self, name: &str) -> Result<Option<SandboxInfo>, ProviderError> {
        self.inner.inspect(name)
    }

    fn exec(&self, name: &str, spec: &ExecSpec) -> Result<Box<dyn Process>, ProviderError> {
        self.log.lock_recovering("exec log").push(spec.argv.clone());
        let writes_managed = spec.argv.get(2).is_some_and(|c| c.contains(MANAGED_DIR));
        if !writes_managed {
            return self.inner.exec(name, spec);
        }
        if self.refuse {
            return Err(ProviderError::Runtime(
                "mkdir: cannot create directory '/etc/claude-code': Permission denied".into(),
            ));
        }
        let command = spec.argv[2].replace(MANAGED_DIR, &self.target.display().to_string());
        self.inner.exec(
            name,
            &ExecSpec {
                argv: vec!["sh".into(), "-c".into(), command],
                cwd: spec.cwd.clone(),
                env: spec.env.clone(),
            },
        )
    }

    fn stop(&self, name: &str) -> Result<(), ProviderError> {
        self.inner.stop(name)
    }

    fn destroy(&self, name: &str) -> Result<(), ProviderError> {
        self.inner.destroy(name)
    }
}

/// Install the intercepting provider for the fixture's yard.
fn install(f: &Fixture, refuse: bool) -> Arc<Intercept> {
    let inner = Arc::new(FakeProvider::plain(
        Box::new(LocalProvider::new()),
        f.dir.join("fake-provider"),
    ));
    let intercept = Arc::new(Intercept {
        inner,
        target: f.dir.join("managed"),
        refuse,
        log: Mutex::new(Vec::new()),
    });
    f.yard.use_sandbox_provider(intercept.clone());
    intercept
}

/// Options for a Claude Code stream-json turn in a sandbox, the fake agent
/// as its harness; `telemetry` as the task asks it.
fn sandboxed(f: &Fixture, record: &Path, telemetry: Option<Telemetry>) -> TaskOptions {
    TaskOptions {
        harness: Some("claude-code-stream-json".into()),
        command: Some(vec![
            fake_agent().display().to_string(),
            "--record-launch".into(),
            record.display().to_string(),
        ]),
        provider: Some(Provider::Microsandbox(SandboxOptions {
            image: "registry.example/harness:1".into(),
            ..SandboxOptions::default()
        })),
        provision: telemetry.map(|telemetry| Provisioning {
            telemetry: Some(telemetry),
            ..Provisioning::default()
        }),
        ..f.options()
    }
}

/// Run one turn with `options` under `policy`.
fn turn(f: &Fixture, name: &str, options: TaskOptions, policy: Policy) -> Branch {
    f.task("hello")
        .options(options)
        .name(name)
        .policy(policy)
        .run()
        .unwrap()
}

/// The managed settings the turn's `Provisioned` activity records.
fn managed_of(events: &[RecordedEvent]) -> Option<ManagedWritten> {
    events.iter().find_map(|e| match &e.activity {
        Activity::Provisioned { managed, .. } => managed.clone(),
        _ => None,
    })
}

fn warnings(events: &[RecordedEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Warning(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// The position of the managed-settings write and of the harness's start
/// in the exec log: the write comes first.
fn positions(execs: &[Vec<String>]) -> (Option<usize>, Option<usize>) {
    let fake = fake_agent().display().to_string();
    let write = execs
        .iter()
        .position(|argv| argv.get(2).is_some_and(|c| c.contains(MANAGED_DIR)));
    let harness = execs
        .iter()
        .position(|argv| argv.first().is_some_and(|program| *program == fake));
    (write, harness)
}

/// The file the write left in the scratch directory, parsed.
fn written(f: &Fixture) -> serde_json::Value {
    let text = fs::read_to_string(f.dir.join("managed").join("managed-settings.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// The denials of `read-only` (the names of its edit, shell and web tools
/// that Claude Code has), with bypass disabled and telemetry on.
#[test]
fn a_sandboxed_read_only_turn_writes_its_denials_before_the_harness_starts() {
    let f = Fixture::new();
    let intercept = install(&f, false);
    let record = f.dir.join("launch.json");
    let branch = turn(
        &f,
        "read-only",
        sandboxed(&f, &record, None),
        PolicyPreset::ReadOnly.policy(),
    );
    // The recording agent exits once it has recorded its launch, so the
    // turn itself ends in a failure the driver reports; only the launch and
    // the settings matter here.
    let events = branch.events().unwrap();
    assert!(record.exists(), "the harness started: {events:?}");

    let (write, harness) = positions(&intercept.execs());
    assert!(write.is_some(), "the managed settings were written");
    assert!(
        write < harness,
        "the write precedes the harness: {write:?} {harness:?}"
    );

    let managed = managed_of(&events).expect("a provisioned activity with managed settings");
    assert_eq!(managed.path, "/etc/claude-code/managed-settings.json");
    assert_eq!(
        managed.deny,
        [
            "Bash",
            "BashOutput",
            "Edit",
            "KillBash",
            "KillShell",
            "MultiEdit",
            "NotebookEdit",
            "WebFetch",
            "WebSearch",
            "Write"
        ]
    );
    assert!(managed.bypass_disabled);
    assert!(!managed.telemetry_off);
    assert_eq!(
        written(&f),
        serde_json::json!({
            "permissions": {
                "deny": [
                    "Bash", "BashOutput", "Edit", "KillBash", "KillShell", "MultiEdit",
                    "NotebookEdit", "WebFetch", "WebSearch", "Write"
                ],
                "disableBypassPermissionsMode": "disable"
            }
        })
    );
    assert!(warnings(&events).is_empty(), "{events:?}");
}

/// `edit-worktree` denies the shell and the web and no edit; a task that
/// turns telemetry off gets the four variables pinned in the file too.
#[test]
fn edit_worktree_with_telemetry_off_pins_it_in_the_file() {
    let f = Fixture::new();
    let intercept = install(&f, false);
    let record = f.dir.join("launch.json");
    let telemetry = Telemetry::parse("off").unwrap();
    let branch = turn(
        &f,
        "edit",
        sandboxed(&f, &record, Some(telemetry)),
        PolicyPreset::EditWorktree.policy(),
    );
    let events = branch.events().unwrap();
    assert!(record.exists(), "{events:?}");
    let (write, harness) = positions(&intercept.execs());
    assert!(write < harness, "{write:?} {harness:?}");
    let managed = managed_of(&events).unwrap();
    assert_eq!(
        managed.deny,
        [
            "Bash",
            "BashOutput",
            "KillBash",
            "KillShell",
            "WebFetch",
            "WebSearch"
        ]
    );
    assert!(managed.telemetry_off);
    assert_eq!(
        written(&f),
        serde_json::json!({
            "env": {
                "CLAUDE_CODE_ENABLE_TELEMETRY": "0",
                "OTEL_LOGS_EXPORTER": "none",
                "OTEL_METRICS_EXPORTER": "none",
                "OTEL_TRACES_EXPORTER": "none"
            },
            "permissions": {
                "deny": [
                    "Bash", "BashOutput", "KillBash", "KillShell", "WebFetch", "WebSearch"
                ],
                "disableBypassPermissionsMode": "disable"
            }
        })
    );
}

/// A write the sandbox refuses is a warning naming the file and why, and the
/// turn goes on: the harness starts, and nothing is recorded as written.
#[test]
fn a_refused_write_is_a_warning_and_the_turn_goes_on() {
    let f = Fixture::new();
    let intercept = install(&f, true);
    let record = f.dir.join("launch.json");
    let branch = turn(
        &f,
        "refused",
        sandboxed(&f, &record, None),
        PolicyPreset::ReadOnly.policy(),
    );
    let events = branch.events().unwrap();
    assert!(record.exists(), "the harness still started: {events:?}");
    let (write, harness) = positions(&intercept.execs());
    assert!(write.is_some() && write < harness, "{write:?} {harness:?}");
    assert_eq!(managed_of(&events), None);
    let warned = warnings(&events);
    assert!(
        warned
            .iter()
            .any(|w| w.contains("managed settings could not be written")
                && w.contains("/etc/claude-code/managed-settings.json")
                && w.contains("Permission denied")),
        "{warned:?}"
    );
    assert!(!f.dir.join("managed").exists(), "nothing was written");
}

/// A local harness is not Branchyard's to configure in the host's `/etc`:
/// no write is made, and the activity records none.
#[test]
fn a_local_branch_gets_no_managed_settings() {
    let f = Fixture::new();
    let intercept = install(&f, false);
    let record = f.dir.join("launch.json");
    let options = TaskOptions {
        harness: Some("claude-code-stream-json".into()),
        command: Some(vec![
            fake_agent().display().to_string(),
            "--record-launch".into(),
            record.display().to_string(),
        ]),
        ..f.options()
    };
    let branch = turn(&f, "local", options, PolicyPreset::ReadOnly.policy());
    let events = branch.events().unwrap();
    assert!(record.exists(), "{events:?}");
    assert!(
        intercept
            .execs()
            .iter()
            .all(|argv| !argv.iter().any(|a| a.contains(MANAGED_DIR))),
        "no write for a local branch"
    );
    assert_eq!(managed_of(&events), None);
    assert!(!f.dir.join("managed").exists());
}
