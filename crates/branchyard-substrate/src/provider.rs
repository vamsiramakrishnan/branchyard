//! [`SubstrateProvider`]: Agent Substrate actors as a [`SandboxProvider`].
//!
//! Lifecycle goes through the `Control` API ([`Actors`]). Exec goes through
//! the router to the Branchyard bridge running in the actor, which the
//! actor's template must start ([`crate::template`]). Every connection to
//! the bridge carries a credential for the actor's current attempt, signed
//! with the host's key; see [`branchyard_bridge::credential`].
//!
//! What it guarantees, beyond the [`SandboxProvider`] contract:
//!
//! - Each [`SandboxProvider::ensure`] and [`SubstrateProvider::begin_attempt`]
//!   starts a new attempt whose credential supersedes every earlier one at the
//!   bridge; [`SubstrateProvider::end_attempt`], `stop` and `destroy` end it,
//!   so it is refused from then on.
//! - Operations on a known actor are bound to its UID: a newer actor that
//!   reused the name is reported, never acted on, and `destroy` cannot delete
//!   it.
//!
//! What it does not guarantee:
//!
//! - Host mounts. An actor cannot see host directories, so a spec with a
//!   mount is refused; code crosses by explicit transfer ([`crate::transfer`]).
//! - Images and limits per sandbox. Both are fixed by the actor template, so
//!   a spec that sets either is refused rather than ignored.
//! - Confidentiality between host and bridge: the router URL is plain HTTP.
//! - Anything about a real cluster. It is exercised only against the fake in
//!   [`crate::fake`].

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use branchyard_bridge::{Claims, Endpoint, Signer};
use branchyard_sandbox::{
    Capabilities, Checkpoint, ExecSpec, Operation, Process, ProviderError, SandboxInfo,
    SandboxProvider, SandboxSpec, SandboxState, SnapshotGuarantee, Unsupported,
};
use tokio::runtime::Runtime;
use tonic::transport::Channel;
use tonic::Code;

use crate::actors::{ActorHandle, Actors, CheckpointRef, Error};

/// How a [`SubstrateProvider`] reaches Substrate and the bridges.
#[derive(Clone, Debug)]
pub struct Config {
    /// The `Control` API, as `http://host:port`. TLS is not supported yet.
    pub endpoint: String,
    pub atespace: String,
    /// The actor template every sandbox is created from.
    pub template: String,
    /// The router URL of an actor's bridge, with `{atespace}` and `{actor}`
    /// in place of the names, such as
    /// `http://router.example/{atespace}/{actor}/`.
    pub router: String,
    /// Signs attempt credentials. Without one, lifecycle calls work and
    /// anything that reaches a bridge fails.
    pub signer: Option<Arc<Signer>>,
    /// How long one attempt's credential lives.
    pub attempt_ttl: Duration,
    /// How long a started actor's bridge may take to answer its health
    /// check through the router.
    pub ready_timeout: Duration,
}

impl Config {
    pub fn new(
        endpoint: impl Into<String>,
        atespace: impl Into<String>,
        template: impl Into<String>,
        router: impl Into<String>,
    ) -> Config {
        Config {
            endpoint: endpoint.into(),
            atespace: atespace.into(),
            template: template.into(),
            router: router.into(),
            signer: None,
            attempt_ttl: Duration::from_secs(6 * 3600),
            ready_timeout: Duration::from_secs(120),
        }
    }

    pub fn signer(mut self, signer: Signer) -> Config {
        self.signer = Some(Arc::new(signer));
        self
    }

    /// The router URL of `actor`'s bridge.
    pub fn router_url(&self, actor: &str) -> String {
        self.router
            .replace("{atespace}", &self.atespace)
            .replace("{actor}", actor)
    }
}

/// An actor this provider knows, and its current attempt, if any.
#[derive(Clone)]
struct Live {
    handle: ActorHandle,
    attempt: Option<Attempt>,
}

#[derive(Clone)]
struct Attempt {
    label: String,
    endpoint: Endpoint,
}

/// Agent Substrate as a synchronous [`SandboxProvider`]. See the module
/// documentation.
pub struct SubstrateProvider {
    runtime: Runtime,
    actors: Actors,
    config: Config,
    live: Mutex<HashMap<String, Live>>,
    last_seq: AtomicU64,
}

impl std::fmt::Debug for SubstrateProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubstrateProvider")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

fn runtime(error: Error) -> ProviderError {
    ProviderError::Runtime(error.to_string())
}

fn unix_now() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

impl SubstrateProvider {
    /// Prepare a provider. Nothing is contacted until the first call.
    pub fn connect(config: Config) -> Result<SubstrateProvider, ProviderError> {
        if !config.router.contains("{actor}") {
            return Err(ProviderError::Invalid(format!(
                "the router URL {} does not name the actor with {{actor}}",
                config.router
            )));
        }
        Endpoint::new(&config.router_url("probe"), "")?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("substrate-provider")
            .enable_all()
            .build()?;
        let channel = {
            let _entered = runtime.enter();
            Channel::from_shared(config.endpoint.clone())
                .map_err(|e| {
                    ProviderError::Invalid(format!("Substrate endpoint {:?}: {e}", config.endpoint))
                })?
                .connect_timeout(Duration::from_secs(30))
                .connect_lazy()
        };
        Ok(SubstrateProvider {
            runtime,
            actors: Actors::new(channel, config.atespace.clone()),
            config,
            live: Mutex::new(HashMap::new()),
            last_seq: AtomicU64::new(0),
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    fn block<T>(
        &self,
        call: impl AsyncFnOnce(&mut Actors) -> Result<T, Error>,
    ) -> Result<T, ProviderError> {
        let mut actors = self.actors.clone();
        self.runtime.block_on(call(&mut actors)).map_err(runtime)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Live>> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The capabilities of the configured template, or an error if it
    /// cannot be read.
    pub fn template_capabilities(&self) -> Result<Capabilities, ProviderError> {
        let template = self.config.template.clone();
        self.block(async move |actors| actors.capabilities(&template).await)
    }

    /// The handle of the actor named `name`, from this provider's memory or
    /// from Substrate.
    pub fn handle(&self, name: &str) -> Result<Option<ActorHandle>, ProviderError> {
        if let Some(live) = self.lock().get(name) {
            return Ok(Some(live.handle.clone()));
        }
        let name = name.to_owned();
        Ok(self
            .block(async move |actors| actors.find(&name).await)?
            .map(|(handle, _)| handle))
    }

    /// A strictly increasing sequence number from the clock, so a later
    /// provider's attempts supersede an earlier one's.
    fn next_seq(&self) -> u64 {
        let now = unix_now().as_micros() as u64;
        let mut last = self.last_seq.load(Ordering::Acquire);
        loop {
            let next = now.max(last + 1);
            match self
                .last_seq
                .compare_exchange(last, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return next,
                Err(seen) => last = seen,
            }
        }
    }

    /// Start a new attempt on the sandbox: mint its credential, which
    /// supersedes the previous attempt's at the bridge, and end the previous
    /// one explicitly if the bridge can be reached.
    pub fn begin_attempt(&self, name: &str, label: &str) -> Result<(), ProviderError> {
        let signer = self.config.signer.clone().ok_or_else(|| {
            ProviderError::Invalid("no signing key is configured for bridge credentials".into())
        })?;
        let handle = self
            .handle(name)?
            .ok_or_else(|| ProviderError::NotFound(name.to_owned()))?;
        let _ = self.end_attempt(name);
        let claims = Claims {
            atespace: handle.atespace.clone(),
            actor: handle.name.clone(),
            uid: handle.uid.clone(),
            attempt: label.to_owned(),
            seq: self.next_seq(),
            expires: (unix_now() + self.config.attempt_ttl).as_secs(),
        };
        let credential = signer
            .sign(&claims)
            .map_err(|e| ProviderError::Invalid(e.to_string()))?;
        let endpoint = Endpoint::new(&self.config.router_url(&handle.name), credential)?;
        self.lock().insert(
            name.to_owned(),
            Live {
                handle,
                attempt: Some(Attempt {
                    label: label.to_owned(),
                    endpoint,
                }),
            },
        );
        Ok(())
    }

    /// End the sandbox's current attempt at its bridge; its credential is
    /// refused from then on and its processes are torn down. Returns their
    /// names. The attempt is forgotten here even if the bridge cannot be
    /// reached; its credential then still expires on its own.
    pub fn end_attempt(&self, name: &str) -> Result<Vec<String>, ProviderError> {
        let attempt = self
            .lock()
            .get_mut(name)
            .and_then(|live| live.attempt.take());
        match attempt {
            None => Ok(Vec::new()),
            Some(attempt) => attempt.endpoint.end_attempt().map_err(|e| {
                ProviderError::Runtime(format!(
                    "could not end attempt {} on {name}: {e}",
                    attempt.label
                ))
            }),
        }
    }

    /// The bridge endpoint of the sandbox's current attempt.
    pub fn endpoint(&self, name: &str) -> Result<Endpoint, ProviderError> {
        let live = self.lock();
        let live = live
            .get(name)
            .ok_or_else(|| ProviderError::NotFound(name.to_owned()))?;
        live.attempt
            .as_ref()
            .map(|a| a.endpoint.clone())
            .ok_or_else(|| {
                ProviderError::Runtime(format!(
                    "sandbox {name} has no current attempt; ensure it first"
                ))
            })
    }

    /// Wait until the sandbox's bridge answers through the router.
    fn wait_ready(&self, name: &str) -> Result<(), ProviderError> {
        let endpoint = self.endpoint(name)?;
        let deadline = Instant::now() + self.config.ready_timeout;
        loop {
            if endpoint.healthy() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(ProviderError::Runtime(format!(
                    "the bridge of {name} did not answer at {} within {}s",
                    endpoint.url(),
                    self.config.ready_timeout.as_secs()
                )));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Write `content` to `path` in the sandbox.
    pub fn put_file(
        &self,
        name: &str,
        path: &Path,
        mode: u32,
        content: &mut dyn Read,
    ) -> Result<(), ProviderError> {
        Ok(self.endpoint(name)?.put_file(path, mode, content)?)
    }

    /// Copy `path` from the sandbox into `out`.
    pub fn get_file(
        &self,
        name: &str,
        path: &Path,
        out: &mut dyn Write,
    ) -> Result<(), ProviderError> {
        Ok(self.endpoint(name)?.get_file(path, out)?)
    }

    fn check(&self, spec: &SandboxSpec) -> Result<(), ProviderError> {
        spec.validate()?;
        if let Some(mount) = spec.mounts.first() {
            return Err(ProviderError::Invalid(format!(
                "a Substrate actor cannot mount host directory {}; transfer code in and out \
                 instead (docs/substrate.md)",
                mount.host.display()
            )));
        }
        if let Some(image) = &spec.image {
            return Err(ProviderError::Invalid(format!(
                "a Substrate actor's image is fixed by template {}, not {image}",
                self.config.template
            )));
        }
        if !spec.resources.is_unlimited() {
            return Err(ProviderError::Invalid(format!(
                "a Substrate actor's limits are fixed by template {}",
                self.config.template
            )));
        }
        Ok(())
    }

    /// Forget and delete the actor, fenced by its UID.
    fn delete(&self, name: &str) -> Result<(), ProviderError> {
        let handle = self.lock().remove(name).map(|live| live.handle);
        let handle = match handle {
            Some(handle) => handle,
            None => {
                let name = name.to_owned();
                match self.block(async move |actors| actors.find(&name).await)? {
                    Some((handle, _)) => handle,
                    None => return Ok(()),
                }
            }
        };
        self.block(async move |actors| actors.destroy(&handle).await)
    }
}

impl SandboxProvider for SubstrateProvider {
    /// The template's declaration; nothing when it cannot be read.
    fn capabilities(&self) -> Capabilities {
        self.template_capabilities().unwrap_or_default()
    }

    fn ensure(&self, spec: &SandboxSpec) -> Result<SandboxInfo, ProviderError> {
        self.check(spec)?;
        let name = spec.name.clone();
        let template = self.config.template.clone();
        let handle = {
            let name = name.clone();
            self.block(async move |actors| actors.ensure(&name, &template).await)?
        };
        self.lock().insert(
            name.clone(),
            Live {
                handle,
                attempt: None,
            },
        );
        self.begin_attempt(&name, &name)?;
        self.wait_ready(&name)?;
        Ok(SandboxInfo {
            name,
            state: SandboxState::Running,
        })
    }

    fn inspect(&self, name: &str) -> Result<Option<SandboxInfo>, ProviderError> {
        let known = self.lock().get(name).map(|live| live.handle.clone());
        let state = match known {
            Some(handle) => {
                let result = self.runtime.block_on({
                    let mut actors = self.actors.clone();
                    async move { actors.inspect(&handle).await }
                });
                match result {
                    Ok(state) => Some(state),
                    Err(Error::Rpc(status)) if status.code() == Code::NotFound => None,
                    Err(error) => return Err(runtime(error)),
                }
            }
            None => {
                let name = name.to_owned();
                self.block(async move |actors| actors.find(&name).await)?
                    .map(|(_, state)| state)
            }
        };
        Ok(state.map(|state| SandboxInfo {
            name: name.to_owned(),
            state,
        }))
    }

    fn exec(&self, name: &str, spec: &ExecSpec) -> Result<Box<dyn Process>, ProviderError> {
        let process = self.endpoint(name)?.exec(spec)?;
        Ok(Box::new(process))
    }

    /// Tear down every process through the bridge, end the attempt, then
    /// suspend the actor. A full-scope suspend would otherwise keep the
    /// processes, frozen, to resume later.
    fn stop(&self, name: &str) -> Result<(), ProviderError> {
        let handle = self
            .handle(name)?
            .ok_or_else(|| ProviderError::NotFound(name.to_owned()))?;
        if let Ok(endpoint) = self.endpoint(name) {
            endpoint.shutdown().map_err(|e| {
                ProviderError::Runtime(format!("could not stop the processes in {name}: {e}"))
            })?;
        }
        let _ = self.end_attempt(name);
        self.block(async move |actors| actors.stop(&handle).await)
    }

    fn destroy(&self, name: &str) -> Result<(), ProviderError> {
        let _ = self.end_attempt(name);
        self.delete(name)
    }

    /// Tag the stopped actor's latest suspend. The actor must already be
    /// stopped: a checkpoint never suspends implicitly.
    fn checkpoint(
        &self,
        name: &str,
        required: &SnapshotGuarantee,
    ) -> Result<Checkpoint, ProviderError> {
        let offered = self.template_capabilities()?.checkpoint;
        let Some(guarantee) = offered.iter().find(|g| g.satisfies(required)).copied() else {
            return Err(ProviderError::Unsupported(Unsupported {
                operation: Operation::Checkpoint,
                required: Some(*required),
                offered,
            }));
        };
        let handle = self
            .handle(name)?
            .ok_or_else(|| ProviderError::NotFound(name.to_owned()))?;
        let tag = format!("{name}-{}", unix_now().as_millis());
        let tag = self.block(async move |actors| actors.checkpoint(&handle, &tag).await)?;
        Ok(Checkpoint {
            sandbox: name.to_owned(),
            reference: tag.name,
            guarantee,
        })
    }

    /// A new, stopped actor from the checkpoint's tag, with its own name,
    /// UID and credentials. [`SandboxProvider::ensure`] with the same name
    /// starts it.
    fn branch(
        &self,
        checkpoint: &Checkpoint,
        spec: &SandboxSpec,
    ) -> Result<SandboxInfo, ProviderError> {
        self.check(spec)?;
        let tag = CheckpointRef {
            atespace: self.config.atespace.clone(),
            name: checkpoint.reference.clone(),
            uid: String::new(),
            snapshot_uri: None,
        };
        let (name, template) = (spec.name.clone(), self.config.template.clone());
        let handle = self.block(async move |actors| actors.branch(&tag, &name, &template).await)?;
        self.lock().insert(
            spec.name.clone(),
            Live {
                handle,
                attempt: None,
            },
        );
        Ok(SandboxInfo {
            name: spec.name.clone(),
            state: SandboxState::Stopped,
        })
    }
}
