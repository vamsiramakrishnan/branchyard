//! [`crate::Provider::Substrate`]: an Agent Substrate actor per turn, the
//! worktree and the private home copied in and back through its bridge.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use branchyard_sandbox::SandboxProvider;
use branchyard_substrate::SubstrateProvider;

use super::ProviderKind;
use crate::placement::{pull_staged, said_back, staging, Placement, SandboxPlan};
use crate::snapshots::Lifecycle;
use crate::state::{Fence, Record};
use crate::{Error, SubstrateOptions, Yard};

impl ProviderKind for SubstrateOptions {
    fn name(&self) -> &'static str {
        "substrate"
    }

    /// A Substrate tag belongs to its atespace and is created from with its
    /// template.
    fn key(&self) -> String {
        format!(
            "substrate:{}/{}/{}",
            self.endpoint.trim_end_matches('/'),
            self.atespace(),
            self.template
        )
    }

    fn lifecycle(&self) -> Option<Lifecycle> {
        Some(Lifecycle::of(self.keep, self.snapshots, self.max_paused))
    }

    fn check(&self, _: &Yard) -> Result<(), Error> {
        check_substrate(self)
    }

    fn guest_paths(&self, _: &Record) -> (String, String) {
        (self.workdir().to_owned(), self.home().to_owned())
    }

    fn prepare(
        &self,
        yard: &Yard,
        record: &Record,
        fence: &Fence,
        plan: &SandboxPlan,
    ) -> Result<Placement, String> {
        Placement::substrate(yard, record, fence, self, plan)
    }

    fn recover(&self, yard: &Yard, record: &Record, sandbox: &str) -> Option<String> {
        Some(recover_actor(yard, record, self, sandbox))
    }

    fn open(&self, _: &Yard) -> Result<Arc<dyn SandboxProvider>, String> {
        substrate_signed(self).map(|p| Arc::new(p) as Arc<dyn SandboxProvider>)
    }

    fn destroy(&self, _: &Yard, sandbox: &str) -> Result<String, String> {
        let provider = substrate_provider(self, false)?;
        SandboxProvider::destroy(&provider, sandbox)
            .map_err(|e| format!("could not delete actor {sandbox}: {e}"))?;
        Ok(format!("deleted actor {sandbox}"))
    }
}

fn check_substrate(options: &SubstrateOptions) -> Result<(), Error> {
    let refuse = |why: String| Err(Error::Unsupported(format!("the substrate provider {why}")));
    // Schemes, loopback-only plain HTTP, and the TLS files.
    if let Err(error) = substrate_config(options).check() {
        return refuse(format!("cannot use its options: {error}"));
    }
    if options.template.trim().is_empty() {
        return refuse("needs an actor template".into());
    }
    for (what, path) in [("workdir", options.workdir()), ("home", options.home())] {
        if !Path::new(path).is_absolute() {
            return refuse(format!("needs an absolute {what}, not {path:?}"));
        }
    }
    branchyard_bridge::Signer::read(&options.key)
        .map(|_| ())
        .or_else(|e| refuse(format!("cannot use its bridge key: {e}")))
}

/// The provider configuration `options` describe, without the key.
fn substrate_config(options: &SubstrateOptions) -> branchyard_substrate::Config {
    let mut config = branchyard_substrate::Config::new(
        &options.endpoint,
        options.atespace(),
        &options.template,
        &options.router,
    );
    config.ca = options.ca.clone();
    config.client_cert = options.client_cert.clone();
    config.client_key = options.client_key.clone();
    config.router_ca = options.router_ca.clone();
    config.insecure = options.insecure;
    config
}

/// A provider for `options`; `signed` includes the bridge key.
pub(crate) fn substrate_provider(
    options: &SubstrateOptions,
    signed: bool,
) -> Result<SubstrateProvider, String> {
    let mut config = substrate_config(options);
    if signed {
        let signer = branchyard_bridge::Signer::read(&options.key).map_err(|e| e.to_string())?;
        config = config.signer(signer);
    }
    config.ready_timeout = Duration::from_secs(300);
    SubstrateProvider::connect(config).map_err(|e| format!("could not reach Substrate: {e}"))
}

/// A provider for `options` that signs bridge credentials.
pub(crate) fn substrate_signed(options: &SubstrateOptions) -> Result<SubstrateProvider, String> {
    substrate_provider(options, true)
}

/// Bring back what the harness left in the actor a stopped engine's turn
/// journaled, if it still exists, as the turn's end would have: a fresh
/// attempt credential from the host key, the actor's working files applied
/// to the worktree only if the worktree still holds exactly what was sent,
/// and the home. Then delete the actor and the transfer's staging
/// directory. Returns what recovery should report.
fn recover_actor(yard: &Yard, record: &Record, options: &SubstrateOptions, actor: &str) -> String {
    let stage = (!actor.is_empty() && !actor.contains(['/', '.'])).then(|| staging(yard, actor));
    let mut said = Vec::new();
    // Deleting needs only the Control API; bringing work back needs the key.
    let deleted = substrate_provider(options, false).and_then(|provider| {
        let existed = provider.handle(actor).map_err(|e| e.to_string())?.is_some();
        if existed {
            said.push(bring_back(record, options, actor, stage.as_deref()));
        }
        provider.destroy(actor).map_err(|e| e.to_string())?;
        Ok(existed)
    });
    if let Some(stage) = &stage {
        let _ = std::fs::remove_dir_all(stage);
    }
    said.push(match deleted {
        Ok(true) => format!("deleted its Substrate actor {actor}"),
        Ok(false) => format!("its Substrate actor {actor} was already gone"),
        Err(error) => format!("could not delete its Substrate actor {actor}: {error}"),
    });
    said.join("; ")
}

/// Pull the worktree and home back from `actor`, and say what happened.
fn bring_back(
    record: &Record,
    options: &SubstrateOptions,
    actor: &str,
    stage: Option<&Path>,
) -> String {
    let pulled = (|| {
        let stage = stage.ok_or("its actor's name cannot name a staging directory")?;
        let provider = substrate_signed(options)?;
        provider
            .begin_attempt(actor, "recovery")
            .map_err(|e| e.to_string())?;
        let endpoint = provider.endpoint(actor).map_err(|e| e.to_string())?;
        let paths = (Path::new(options.workdir()), Path::new(options.home()));
        let pulled = pull_staged(&endpoint, record, paths, stage);
        let _ = provider.end_attempt(actor);
        pulled
    })();
    said_back(pulled, "actor", actor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substrate_options_are_checked_before_anything_is_created() {
        let dir = tempfile::Builder::new()
            .prefix("by-placement-")
            .tempdir()
            .unwrap();
        let key = dir.path().join("key");
        branchyard_bridge::Signer::write(&key).unwrap();
        let good = SubstrateOptions {
            endpoint: "http://127.0.0.1:9".into(),
            router: "http://127.0.0.1:9/{atespace}/{actor}/".into(),
            template: "by".into(),
            key: key.clone(),
            ..SubstrateOptions::default()
        };
        assert!(check_substrate(&good.clone()).is_ok());
        assert!(good.sandboxed());
        // TLS anywhere, or plain HTTP to another host when asked for.
        for fine in [
            SubstrateOptions {
                endpoint: "https://control.example".into(),
                router: "wss://router.example/{atespace}/{actor}/".into(),
                ..good.clone()
            },
            SubstrateOptions {
                endpoint: "http://control.example:8080".into(),
                insecure: true,
                ..good.clone()
            },
        ] {
            assert!(check_substrate(&fine.clone()).is_ok(), "{fine:?}");
        }
        let bad = [
            SubstrateOptions {
                endpoint: "ftp://control".into(),
                ..good.clone()
            },
            SubstrateOptions {
                endpoint: "http://control.example:8080".into(),
                ..good.clone()
            },
            SubstrateOptions {
                router: "ws://router.example/{actor}/".into(),
                ..good.clone()
            },
            SubstrateOptions {
                endpoint: "https://control.example".into(),
                ca: Some(dir.path().join("missing-ca.pem")),
                ..good.clone()
            },
            SubstrateOptions {
                endpoint: "https://control.example".into(),
                client_cert: Some(key.clone()),
                ..good.clone()
            },
            SubstrateOptions {
                router: "http://router/".into(),
                ..good.clone()
            },
            SubstrateOptions {
                template: " ".into(),
                ..good.clone()
            },
            SubstrateOptions {
                workdir: "relative".into(),
                ..good.clone()
            },
            SubstrateOptions {
                key: dir.path().join("missing"),
                ..good.clone()
            },
        ];
        for options in bad {
            assert!(
                matches!(
                    check_substrate(&options.clone()),
                    Err(Error::Unsupported(_))
                ),
                "{options:?}"
            );
        }
    }
}
