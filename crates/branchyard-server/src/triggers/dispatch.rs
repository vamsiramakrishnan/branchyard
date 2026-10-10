//! The trigger dispatcher inside a server or `by worker`: the [`Hub`]
//! routes and the dispatcher share, the [`AppSink`] that fires through the
//! server's own admission path, and the loop that ticks the [`Engine`].

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use branchyard::{BranchStatus, Fleet, RouteOptions, SteerState, TaskKind};
use branchyard_client::api::{Operation, SendRequest, TaskRequest};
use branchyard_client::triggers::Busy;
use tokio::sync::{watch, Notify};

use super::engine::{Admitted, Delivered, Engine, Refusal, Sink};
use super::store::TriggerStore;
use super::{Settings, StoredTrigger};
use crate::api::{App, Caller, RepoState};
use crate::store::Idempotency;

/// What the routes and the dispatcher share.
pub struct Hub {
    pub store: Arc<dyn TriggerStore>,
    pub settings: Settings,
    /// Wakes the dispatcher when a webhook recorded a run.
    pub wake: Notify,
    /// Where webhook senders reach this server.
    pub base_url: String,
    /// This process's name on its claims.
    pub worker: String,
}

impl Hub {
    pub fn new(store: Arc<dyn TriggerStore>, settings: Settings, base_url: String) -> Hub {
        Hub {
            store,
            base_url: settings.public_url.clone().unwrap_or(base_url),
            settings,
            wake: Notify::new(),
            worker: format!("t_{}", &branchyard_client::new_key()[..20]),
        }
    }
}

/// Fires through `app`: its repositories, registry, admission and
/// configuration.
pub struct AppSink {
    pub app: Arc<App>,
    pub runtime: tokio::runtime::Handle,
}

/// The idempotency scope of a trigger's tasks: unique per trigger, and
/// unlike any credential's subject.
pub fn idempotency_scope(trigger: &StoredTrigger) -> String {
    format!("trigger:{}/{}", trigger.tenant, trigger.id)
}

/// How long a run that steers waits for the engine running the turn to
/// take the input, as `POST .../steer` does.
const STEER_WAIT: Duration = Duration::from_secs(10);

impl AppSink {
    fn repo(&self, name: &str) -> Result<&RepoState, String> {
        self.app
            .repos
            .get(name)
            .ok_or_else(|| super::target::NOT_SERVED.to_owned())
    }

    /// The operation the run's key already names, if an earlier attempt
    /// admitted one: a run fired again after its dispatcher stopped finds
    /// it, whatever it rendered.
    fn replayed(&self, trigger: &StoredTrigger, key: &str) -> Result<Option<Admitted>, Refusal> {
        let scope = idempotency_scope(trigger);
        let existing = self
            .app
            .registry
            .by_key(&scope, key, &trigger.tenant)
            .map_err(|e| Refusal::Later(e.body.message.clone()))?;
        Ok(existing.map(|op| Admitted {
            operation: op.id,
            branches: op.branches,
        }))
    }

    /// The idempotency key of the run's operation, bound to what it sends.
    fn idempotency(&self, trigger: &StoredTrigger, key: &str, canonical: &str) -> Idempotency {
        Idempotency {
            caller: idempotency_scope(trigger),
            key: key.to_owned(),
            fingerprint: crate::api::fingerprint(&format!("trigger\n{canonical}")),
        }
    }

    /// What an admission came to: the operation, the one another attempt
    /// bound the key to first, or the refusal.
    fn admitted(
        &self,
        trigger: &StoredTrigger,
        key: &str,
        admitted: Result<(Operation, bool), crate::error::ApiError>,
    ) -> Result<Admitted, Refusal> {
        match admitted {
            Ok((op, _)) => Ok(Admitted {
                operation: op.id,
                branches: op.branches,
            }),
            Err(e) => {
                // Another attempt bound the key first with a different
                // rendering: that one is the run's task.
                if let Ok(Some(admitted)) = self.replayed(trigger, key) {
                    return Ok(admitted);
                }
                let why = format!("{}: {}", e.body.code, e.body.message);
                // A stopping server is not the trigger's failure.
                match e.body.code.as_str() {
                    "shutting_down" => Err(Refusal::Later(why)),
                    _ => Err(Refusal::Failed(why)),
                }
            }
        }
    }
}

impl Sink for AppSink {
    fn repos(&self) -> Vec<(String, PathBuf)> {
        self.app
            .repos
            .values()
            .map(|r| (r.name.clone(), r.yard.root().to_path_buf()))
            .collect()
    }

    fn admit(
        &self,
        trigger: &StoredTrigger,
        key: &str,
        request: TaskRequest,
    ) -> Result<Admitted, Refusal> {
        if let Some(admitted) = self.replayed(trigger, key)? {
            return Ok(admitted);
        }
        let canonical =
            serde_json::to_string(&request).map_err(|e| Refusal::Failed(e.to_string()))?;
        let idem = self.idempotency(trigger, key, &canonical);
        let caller = Caller(trigger.principal.clone());
        let app = self.app.clone();
        let repo = trigger.spec.repo.clone();
        let admitted = self.runtime.block_on(async move {
            let repo = app.authorized_repo(&caller, &repo, "run")?.clone();
            crate::api::admit_task(&app, &repo, &caller, Some(idem), request, None).await
        });
        self.admitted(trigger, key, admitted)
    }

    fn deliver(
        &self,
        trigger: &StoredTrigger,
        key: &str,
        request: TaskRequest,
        busy: Busy,
    ) -> Result<Delivered, Refusal> {
        if let Some(admitted) = self.replayed(trigger, key)? {
            return Ok(Delivered::Admitted(admitted));
        }
        let branch = request.name.clone().unwrap_or_default();
        let request_name = branch.clone();
        let yard = self
            .repo(&trigger.spec.repo)
            .map_err(Refusal::Failed)?
            .yard
            .clone();
        let target = match yard.branch(&branch) {
            Ok(target) => target,
            // Missing: the task creates it, under the delivered name.
            Err(branchyard::Error::UnknownBranch(_)) => {
                return self.admit(trigger, key, request).map(Delivered::Admitted)
            }
            Err(e) => return Err(Refusal::Failed(e.to_string())),
        };
        let running = target.info().status == BranchStatus::Running;
        match (running, busy) {
            (true, Busy::Skip) => return Ok(Delivered::Busy),
            (true, Busy::Steer) => {
                let by = format!("trigger {}", trigger.spec.name);
                let steered = yard
                    .steer_as(&branch, &request.prompt, &by)
                    .and_then(|s| yard.wait_steer(&branch, s.id, STEER_WAIT));
                match steered {
                    Ok(steer) if !matches!(steer.state, SteerState::Refused { .. }) => {
                        return Ok(Delivered::Steered)
                    }
                    // The turn ended first, refused the input, or its
                    // harness takes none mid-turn: a send runs after it.
                    Ok(_)
                    | Err(branchyard::Error::NotRunning(_))
                    | Err(branchyard::Error::Unsupported(_)) => {}
                    Err(e) => return Err(Refusal::Failed(e.to_string())),
                }
            }
            _ => {}
        }
        // What the task would have given a new branch, the send gives this
        // one; its harness, base and name are the branch's own.
        let send = SendRequest {
            prompt: request.prompt,
            budget: request.budget,
            policy: request.policy,
            check: request.check,
            command: request.command,
            delegation: request.delegation,
            allow_delegation: request.allow_delegation,
            unapproved_tools: request.unapproved_tools,
            provision: request.provision,
            require_labels: request.require_labels,
            priority: request.priority,
            ..SendRequest::default()
        };
        let canonical = serde_json::to_string(&send).map_err(|e| Refusal::Failed(e.to_string()))?;
        let idem = self.idempotency(trigger, key, &canonical);
        let caller = Caller(trigger.principal.clone());
        let app = self.app.clone();
        let repo = trigger.spec.repo.clone();
        let admitted = self.runtime.block_on(async move {
            let repo = app.authorized_repo(&caller, &repo, "run")?.clone();
            crate::api::admit_send(&app, &repo, &caller, Some(idem), branch, send, None).await
        });
        // The server admits one operation on a branch at a time: taken
        // meanwhile (a turn started since the look above, or another
        // send), the run does what its policy says, now that it knows.
        if let Err(e) = &admitted {
            if e.body.code == "branch_busy" {
                return match busy {
                    Busy::Skip => Ok(Delivered::Busy),
                    Busy::Queue | Busy::Steer => Err(Refusal::Later(format!(
                        "{request_name} is running a turn; the send waits for it"
                    ))),
                };
            }
        }
        self.admitted(trigger, key, admitted)
            .map(Delivered::Admitted)
    }

    fn operation(&self, id: &str) -> Option<Operation> {
        self.app.registry.get(id).ok().flatten()
    }

    fn prechecks_allowed(&self, repo: &str) -> bool {
        self.app.config.triggers.allow_prechecks.allows(repo)
    }

    fn route(
        &self,
        trigger: &StoredTrigger,
        mut request: TaskRequest,
        seed: u64,
    ) -> Result<TaskRequest, String> {
        let repo = self.repo(&trigger.spec.repo)?;
        let fleet = fleet_of(repo)?;
        let options = crate::work::task_options(&self.app, repo, &request)
            .map_err(|e| e.body.message.clone())?;
        let kind = match trigger.spec.route.as_ref().and_then(|r| r.kind.as_deref()) {
            Some(kind) => Some(kind.parse::<TaskKind>().map_err(|e| e.to_string())?),
            None => None,
        };
        let how = RouteOptions {
            kind,
            seed: Some(seed),
            attempts: Some(1),
            failover: Some(false),
            excluded: Default::default(),
            harnesses: self
                .app
                .registry
                .live_workers()
                .ok()
                .map(|workers| crate::store::WorkersGate::new(&repo.name, workers).into_arc()),
        };
        let route = repo
            .yard
            .route(&request.prompt, &options, &fleet, &how, Some(1))
            .map_err(|e| e.to_string())?;
        let pick = route
            .picks
            .first()
            .ok_or("the fleet table picked no candidate")?;
        request.harness = Some(pick.candidate.harness.clone());
        if pick.candidate.model.is_some() || pick.candidate.effort.is_some() {
            let mut provision = request.provision.take().unwrap_or_default();
            if let Some(model) = &pick.candidate.model {
                provision.model = Some(model.clone());
            }
            if pick.candidate.effort.is_some() {
                provision.effort = pick.candidate.effort;
            }
            request.provision = Some(provision);
        }
        tracing::info!(
            trigger = %trigger.spec.name,
            kind = %route.kind,
            harness = %pick.candidate.harness,
            reason = %pick.reason,
            "routed a trigger's task"
        );
        Ok(request)
    }

    fn scratch(&self) -> PathBuf {
        self.app.config.data_dir.join("triggers")
    }
}

/// The `[fleet]` of `repo`'s branchyard.toml, as the SDK routes with it.
fn fleet_of(repo: &RepoState) -> Result<Fleet, String> {
    let path = repo
        .yard
        .root()
        .join(branchyard_setup::config::PROJECT_FILE);
    let text = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "a routed trigger needs a [fleet] in {}: {e}",
            path.display()
        )
    })?;
    let config =
        branchyard_setup::config::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    if config.fleet.is_empty() {
        return Err(format!(
            "a routed trigger needs a [fleet] in {}",
            path.display()
        ));
    }
    let mut entries = std::collections::BTreeMap::new();
    for (kind, entry) in &config.fleet {
        let mut candidates = Vec::new();
        for c in &entry.candidates {
            candidates.push(branchyard::FleetCandidate {
                harness: c.harness.clone(),
                model: c.model.clone(),
                effort: c
                    .effort
                    .as_deref()
                    .map(branchyard::Effort::parse)
                    .transpose()
                    .map_err(|e| format!("fleet.{kind}: {e}"))?,
                // A candidate's command is the repository's to choose; a
                // server uses its own harness_commands.
                command: None,
            });
        }
        entries.insert(
            kind.clone(),
            branchyard::FleetEntry {
                candidates,
                attempts: 1,
                budget: branchyard::Budget {
                    max_usd: entry.budget_usd,
                    max_turns: entry.max_turns,
                    max_duration: entry
                        .max_minutes
                        .and_then(|m| Duration::try_from_secs_f64(m * 60.0).ok()),
                    ..branchyard::Budget::default()
                },
                judge: None,
                failover: false,
                exploration: entry.exploration.unwrap_or(branchyard::DEFAULT_EXPLORATION),
                environment: entry.environment.clone(),
                connectors: entry.connectors.clone(),
                // A trigger's branch has no one waiting to approve a plan.
                plan: false,
                goal_judge: None,
            },
        );
    }
    Ok(Fleet { entries })
}

/// Tick the dispatcher until shutdown: every `settings.tick`, and at once
/// when a webhook records a run.
pub fn spawn(app: Arc<App>, mut shutdown: watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let hub = app.triggers.clone();
        let engine = Arc::new(Engine {
            store: hub.store.clone(),
            clock: hub.settings.clock.clone(),
            sink: Arc::new(AppSink {
                app: app.clone(),
                runtime: tokio::runtime::Handle::current(),
            }),
            worker: hub.worker.clone(),
        });
        loop {
            tokio::select! {
                _ = hub.wake.notified() => {}
                _ = tokio::time::sleep(hub.settings.tick) => {}
                _ = shutdown.changed() => {}
            }
            if *shutdown.borrow() {
                return;
            }
            let ticking = engine.clone();
            let claimed = tokio::task::spawn_blocking(move || {
                let mut claimed = ticking.claim_due()?;
                while let Some(run) = ticking.claim_pending()? {
                    claimed.push(run);
                }
                if let Err(e) = ticking.settle() {
                    tracing::error!(error = %e, "settling trigger runs");
                }
                Ok::<_, std::io::Error>(claimed)
            })
            .await;
            let claimed = match claimed {
                Ok(Ok(claimed)) => claimed,
                Ok(Err(e)) => {
                    tracing::error!(error = %e, "claiming trigger runs");
                    continue;
                }
                Err(_) => return,
            };
            // Each run on a thread of its own: a precheck may take minutes.
            for (run, fence) in claimed {
                let firing = engine.clone();
                tokio::task::spawn_blocking(move || match firing.fire(run, fence) {
                    Ok(run) => tracing::info!(
                        trigger = %run.trigger,
                        run = %run.id,
                        state = %run.state.as_str(),
                        reason = %run.reason.as_deref().unwrap_or(""),
                        operation = %run.operation.as_deref().unwrap_or(""),
                        "trigger run"
                    ),
                    Err(e) => tracing::error!(error = %e, "firing a trigger run"),
                });
            }
        }
    })
}
