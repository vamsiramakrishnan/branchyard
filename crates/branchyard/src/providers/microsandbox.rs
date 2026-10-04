//! [`crate::Provider::Microsandbox`]: a Microsandbox microVM per turn, with
//! the worktree and the private home mounted into it.

use branchyard_support::best_effort;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::sync::Arc;

use branchyard_sandbox::{Mount, Resources, SandboxProvider, SandboxSpec};

use super::ProviderKind;
use crate::placement::{
    sandbox_env, sandbox_name, Placement, Planned, SandboxPlan, HOME, SCRATCH_MOUNT_BASE, WORKSPACE,
};
use crate::snapshots::Lifecycle;
use crate::state::{Fence, Record};
use crate::{git, Error, SandboxOptions, Yard};
use branchyard_support::time::now_ms;

impl ProviderKind for SandboxOptions {
    fn name(&self) -> &'static str {
        "microsandbox"
    }

    fn lifecycle(&self) -> Option<Lifecycle> {
        Some(Lifecycle::of(self.keep, self.snapshots, self.max_paused))
    }

    /// A yard given its own sandbox provider ([`Yard::use_sandbox_provider`])
    /// runs Microsandbox branches without the SDK.
    fn check(&self, yard: &Yard) -> Result<(), Error> {
        let own = crate::projection::lock(&yard.hub.sandbox_provider).is_some();
        if !own && !branchyard_microsandbox::ENABLED {
            return Err(Error::Unsupported(
                "this build has no Microsandbox support; rebuild with \
                 --features microsandbox (Rust 1.94 or newer), see docs/providers.md"
                    .into(),
            ));
        }
        if self.image.trim().is_empty() {
            return Err(Error::Unsupported(
                "the microsandbox provider needs an image".into(),
            ));
        }
        Ok(())
    }

    fn guest_paths(&self, _: &Record) -> (String, String) {
        (WORKSPACE.into(), HOME.into())
    }

    fn prepare(
        &self,
        yard: &Yard,
        record: &Record,
        fence: &Fence,
        plan: &SandboxPlan,
    ) -> Result<Placement, String> {
        Placement::microsandbox(yard, record, fence, self, plan)
    }

    fn fan_spec(&self, yard: &Yard, record: &Record) -> Result<Option<Planned>, String> {
        let (spec, _) = spec(yard, record, self)?;
        Ok(Some((spec, microsandbox(yard, self)?)))
    }

    fn recover(&self, yard: &Yard, _: &Record, sandbox: &str) -> Option<String> {
        Some(destroy_orphan(microsandbox(yard, self), sandbox))
    }

    fn open(&self, yard: &Yard) -> Result<Arc<dyn SandboxProvider>, String> {
        microsandbox(yard, self)
    }

    fn destroy(&self, yard: &Yard, sandbox: &str) -> Result<String, String> {
        let said = destroy_orphan(microsandbox(yard, self), sandbox);
        match said.starts_with("could not") {
            true => Err(said),
            false => Ok(said),
        }
    }
}

/// The spec of a Microsandbox branch's sandbox, and its harness's
/// variables: the worktree at [`WORKSPACE`], the private home at [`HOME`],
/// the git directory read-only, and every scratch area it may reach.
pub(crate) fn spec(
    yard: &Yard,
    record: &Record,
    options: &SandboxOptions,
) -> Result<(SandboxSpec, BTreeMap<OsString, OsString>), String> {
    let home = record
        .home
        .clone()
        .ok_or("a sandboxed branch has no private home")?;
    let mut env = sandbox_env(HOME, &options.pass_env)?;
    let git_dir = git::common_dir(&yard.root).map_err(|e| e.to_string())?;
    let mut mounts = vec![
        Mount::writable(&record.info.worktree, WORKSPACE),
        Mount::writable(home, HOME),
        Mount::read_only(&git_dir, &git_dir),
    ];
    // Every scratch area this branch may reach is mounted read-write at a
    // fixed guest path, named for the harness the same way the local
    // provider's environment variable does; see `docs/storage.md`. One
    // writer at a time is still enforced by `by scratch lock`, not by this
    // mount.
    if let Ok(areas) = crate::storage::authorized_scratch(yard, &record.info.name) {
        for area in &areas {
            let host = crate::storage::scratch_dir(&yard.store(), &area.name);
            best_effort(
                "create the sandbox's mount directory",
                std::fs::create_dir_all(&host),
            );
            let guest = format!("{SCRATCH_MOUNT_BASE}/{}", area.name);
            mounts.push(Mount::writable(&host, &guest));
            env.insert(
                crate::storage::scratch_env_var(&area.name).into(),
                guest.into(),
            );
        }
    }
    let keep = options.lifecycle().is_some_and(|l| l.keep);
    let spec = SandboxSpec {
        name: sandbox_name(&record.info.name, now_ms()),
        image: Some(options.image.clone()),
        resources: Resources {
            cpus: options.cpus,
            memory_mib: options.memory_mib,
        },
        mounts,
        // A kept sandbox outlives this process.
        persist: keep,
    };
    Ok((spec, env))
}

/// The provider a Microsandbox branch runs through: the yard's own
/// ([`Yard::use_sandbox_provider`]), or the SDK's, with live branching
/// declared only when `options` opt in.
pub(crate) fn microsandbox(
    yard: &Yard,
    options: &SandboxOptions,
) -> Result<Arc<dyn SandboxProvider>, String> {
    if let Some(own) = crate::projection::lock(&yard.hub.sandbox_provider).clone() {
        return Ok(own);
    }
    sdk(options.live_branch)
}

#[cfg(feature = "microsandbox")]
fn sdk(live_branch: bool) -> Result<Arc<dyn SandboxProvider>, String> {
    branchyard_microsandbox::MicrosandboxProvider::new()
        .map(|p| Arc::new(p.with_live_branch(live_branch)) as Arc<dyn SandboxProvider>)
        .map_err(|e| format!("could not start the Microsandbox SDK: {e}"))
}

#[cfg(not(feature = "microsandbox"))]
fn sdk(_: bool) -> Result<Arc<dyn SandboxProvider>, String> {
    Err("this build has no Microsandbox support".into())
}

/// Destroy the Microsandbox sandbox `name` a stopped engine left, through
/// `provider`, and say what happened.
pub(crate) fn destroy_orphan(
    provider: Result<Arc<dyn SandboxProvider>, String>,
    name: &str,
) -> String {
    let destroyed = provider.and_then(|provider| {
        let existed = provider.inspect(name).map_err(|e| e.to_string())?.is_some();
        provider.destroy(name).map_err(|e| e.to_string())?;
        Ok(existed)
    });
    match destroyed {
        Ok(true) => format!("destroyed its Microsandbox sandbox {name}"),
        Ok(false) => format!("its Microsandbox sandbox {name} was already gone"),
        Err(error) => format!("could not destroy its Microsandbox sandbox {name}: {error}"),
    }
}

#[allow(clippy::unwrap_in_result)] // tests: a panic is the failure report
#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use serde_json::json;

    /// A stand-in for the Microsandbox provider, which this build may not
    /// have: it knows some sandboxes, and records what it destroys. Clones
    /// share their state.
    #[derive(Clone, Default)]
    pub(crate) struct Standin {
        pub(crate) known: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        pub(crate) destroyed: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        pub(crate) fail: bool,
    }

    impl SandboxProvider for Standin {
        fn capabilities(&self) -> branchyard_sandbox::Capabilities {
            branchyard_sandbox::Capabilities::default()
        }
        fn ensure(
            &self,
            _: &SandboxSpec,
        ) -> Result<branchyard_sandbox::SandboxInfo, branchyard_sandbox::ProviderError> {
            unreachable!("recovery creates nothing")
        }
        fn inspect(
            &self,
            name: &str,
        ) -> Result<Option<branchyard_sandbox::SandboxInfo>, branchyard_sandbox::ProviderError>
        {
            let known = self.known.lock().unwrap().iter().any(|n| n == name);
            Ok(known.then(|| branchyard_sandbox::SandboxInfo {
                name: name.into(),
                state: branchyard_sandbox::SandboxState::Running,
            }))
        }
        fn exec(
            &self,
            _: &str,
            _: &branchyard_sandbox::ExecSpec,
        ) -> Result<Box<dyn branchyard_sandbox::Process>, branchyard_sandbox::ProviderError>
        {
            unreachable!("recovery runs nothing")
        }
        fn stop(&self, _: &str) -> Result<(), branchyard_sandbox::ProviderError> {
            unreachable!("recovery destroys")
        }
        fn destroy(&self, name: &str) -> Result<(), branchyard_sandbox::ProviderError> {
            if self.fail {
                return Err(branchyard_sandbox::ProviderError::Runtime(
                    "the VM would not stop".into(),
                ));
            }
            self.known.lock().unwrap().retain(|n| n != name);
            self.destroyed.lock().unwrap().push(name.into());
            Ok(())
        }
    }

    #[test]
    fn a_microsandbox_sandbox_left_by_a_stopped_engine_is_destroyed_through_its_provider() {
        let standin = |fail| Standin {
            known: std::sync::Arc::new(std::sync::Mutex::new(vec!["by-x-1".into()])),
            destroyed: Default::default(),
            fail,
        };
        let boxed =
            |p: &Standin| -> Result<Arc<dyn SandboxProvider>, String> { Ok(Arc::new(p.clone())) };
        let provider = standin(false);
        assert_eq!(
            destroy_orphan(boxed(&provider), "by-x-1"),
            "destroyed its Microsandbox sandbox by-x-1"
        );
        assert_eq!(*provider.destroyed.lock().unwrap(), ["by-x-1"]);
        assert_eq!(
            destroy_orphan(boxed(&provider), "by-x-1"),
            "its Microsandbox sandbox by-x-1 was already gone"
        );
        assert_eq!(
            destroy_orphan(boxed(&standin(true)), "by-x-1"),
            "could not destroy its Microsandbox sandbox by-x-1: sandbox runtime: the VM would \
             not stop"
        );
        if !branchyard_microsandbox::ENABLED {
            assert_eq!(
                destroy_orphan(sdk(false), "by-x-1"),
                "could not destroy its Microsandbox sandbox by-x-1: this build has no \
                 Microsandbox support"
            );
        }
    }

    /// Recovery leaves a sandbox its turn had parked (its record is how the
    /// next turn finds it), and destroys one it had not, removing any kept
    /// record of it.
    #[test]
    fn recovery_keeps_a_parked_sandbox_and_destroys_an_unparked_one() {
        use crate::state::{SandboxKind, SandboxRow};
        let dir = tempfile::Builder::new()
            .prefix("by-placement-")
            .tempdir()
            .unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(dir.path())
            .status()
            .unwrap();
        let yard = Yard::open(dir.path()).unwrap();
        let standin = Standin {
            known: std::sync::Arc::new(std::sync::Mutex::new(vec!["by-b-1".into()])),
            destroyed: Default::default(),
            fail: false,
        };
        yard.use_sandbox_provider(Arc::new(standin.clone()));
        let record: Record = serde_json::from_value(json!({
            "info": {
                "name": "b", "git_branch": "by/b", "worktree": "/w",
                "prompt": "p", "harness": "h", "profile": "p", "session": null,
                "parent": null, "base": "b", "candidate": null,
                "status": {"state": "running"}, "turns": 0, "cost_usd": null,
                "created_at": 0
            },
            "created_ms": 0, "check": null, "command": null, "home": null,
            "cost_baseline": null,
            "provider": {"kind": "microsandbox", "image": "i", "keep": "pause"}
        }))
        .unwrap();
        let intent = json!({ "provider": "microsandbox", "sandbox": "by-b-1" });
        let kept = json!({ "kept": true });
        assert_eq!(
            crate::placement::recover(&yard, &record, &intent, Some(&kept)).as_deref(),
            Some("its sandbox by-b-1 was already kept paused for the next turn")
        );
        assert!(standin.destroyed.lock().unwrap().is_empty());
        yard.store()
            .sandboxes()
            .put_sandbox(&SandboxRow {
                branch: "b".into(),
                incarnation: 1,
                kind: SandboxKind::Kept,
                provider: "microsandbox".into(),
                name: "by-b-1".into(),
                turn: None,
                detail: "{}".into(),
                used_ms: 1,
            })
            .unwrap();
        assert_eq!(
            crate::placement::recover(&yard, &record, &intent, None).as_deref(),
            Some("destroyed its Microsandbox sandbox by-b-1")
        );
        assert_eq!(*standin.destroyed.lock().unwrap(), ["by-b-1"]);
        assert!(yard.store().sandboxes().sandboxes("b").unwrap().is_empty());
    }
}
