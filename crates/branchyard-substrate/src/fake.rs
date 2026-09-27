//! An in-process fake of a Substrate cluster, for hermetic tests: the
//! `Control` API over real gRPC, a router, and actors whose one process is a
//! real `branchyard-bridge` running on this host.
//!
//! It models only the documented behavior Branchyard depends on:
//!
//! - Actors are created suspended with a new UID, resumed to running,
//!   suspended, reverted and deleted (with an optional UID precondition). An
//!   actor from a template that runs the bridge ([`crate::runs_bridge`]) runs
//!   one while it is running: started on resume with the template's key and
//!   the actor's identity files, stopped (with every process it started) on
//!   suspend, revert and delete. Its attempt state lives in a per-actor
//!   directory that survives suspend, as a root filesystem would.
//! - Tags copy a suspended actor's directory; an actor created from a tag
//!   starts from that copy, with its own UID.
//! - The router forwards `/actors/<atespace>/<actor>/<rest>` to the running
//!   actor's bridge as `/<rest>`, WebSocket upgrades included, and answers
//!   503 for an actor that is not running.
//!
//! Nothing here is evidence about a real cluster: the real router's
//! addressing, activation and authentication are not modelled.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tokio::runtime::Runtime;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;
use tonic::{Request, Response, Status};

use crate::pb;
use crate::pb::control_server::{Control, ControlServer};

struct Bridge {
    child: Child,
    address: SocketAddr,
}

impl Bridge {
    /// Close its lifeline and wait for it to tear down and exit.
    fn stop(mut self) {
        drop(self.child.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Actor {
    proto: pb::Actor,
    /// Stands in for the actor's root filesystem.
    root: PathBuf,
    bridge: Option<Bridge>,
}

#[derive(Default)]
struct State {
    templates: HashMap<String, pb::ActorTemplate>,
    actors: HashMap<String, Actor>,
    tags: HashMap<String, (pb::Tag, PathBuf)>,
    next_uid: u32,
}

struct Inner {
    atespace: String,
    state: Mutex<State>,
    bridge: Option<PathBuf>,
    scratch: PathBuf,
    /// Paths removed whenever an actor is created, standing in for a fresh
    /// root filesystem.
    fresh: Mutex<Vec<PathBuf>>,
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn name_of(&self, reference: Option<pb::ObjectRef>) -> Result<String, Status> {
        let reference = reference.ok_or_else(|| Status::invalid_argument("missing reference"))?;
        if reference.atespace != self.atespace {
            return Err(Status::permission_denied("wrong atespace"));
        }
        Ok(reference.name)
    }

    fn start_bridge(&self, actor: &pb::Actor, root: &Path, key: &str) -> Result<Bridge, Status> {
        let program = self
            .bridge
            .as_ref()
            .ok_or_else(|| Status::internal("this fake has no bridge binary"))?;
        let metadata = actor.metadata.clone().unwrap_or_default();
        let identity = root.join("identity");
        fs::create_dir_all(&identity).map_err(|e| Status::internal(e.to_string()))?;
        for (file, value) in [
            ("atespace", &metadata.atespace),
            ("name", &metadata.name),
            ("uid", &metadata.uid),
        ] {
            fs::write(identity.join(file), format!("{value}\n"))
                .map_err(|e| Status::internal(e.to_string()))?;
        }
        let mut child = Command::new(program)
            .args(["serve", "--listen", "127.0.0.1:0", "--lifeline-stdin"])
            .arg("--identity")
            .arg(&identity)
            .arg("--state")
            .arg(root.join("state/attempts"))
            .env(branchyard_bridge::KEY_ENV, key)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| Status::internal(format!("could not start the bridge: {e}")))?;
        let mut line = String::new();
        let _ = BufReader::new(child.stdout.take().expect("piped")).read_line(&mut line);
        let address = line
            .trim()
            .strip_prefix("listening ")
            .and_then(|a| a.parse().ok());
        match address {
            Some(address) => Ok(Bridge { child, address }),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                Err(Status::internal(format!(
                    "the bridge did not start: {line:?}"
                )))
            }
        }
    }

    fn transition(
        &self,
        reference: Option<pb::ObjectRef>,
        to: pb::ActorState,
    ) -> Result<pb::Actor, Status> {
        let name = self.name_of(reference)?;
        let stopped = {
            let mut state = self.lock();
            let template_key = {
                let actor = state
                    .actors
                    .get(&name)
                    .ok_or_else(|| Status::not_found(name.clone()))?;
                let template = actor
                    .proto
                    .actor_template
                    .as_ref()
                    .map(|t| t.name.clone())
                    .unwrap_or_default();
                state.templates.get(&template).and_then(bridge_key)
            };
            let actor = state.actors.get_mut(&name).expect("checked above");
            set_state(&mut actor.proto, to);
            match to {
                pb::ActorState::Running => {
                    if let (None, Some(key)) = (&actor.bridge, template_key) {
                        actor.bridge = Some(self.start_bridge(&actor.proto, &actor.root, &key)?);
                    }
                    None
                }
                _ => actor.bridge.take(),
            }
        };
        if let Some(bridge) = stopped {
            bridge.stop();
        }
        let state = self.lock();
        Ok(state.actors[&name].proto.clone())
    }
}

fn bridge_key(template: &pb::ActorTemplate) -> Option<String> {
    template
        .containers
        .iter()
        .flat_map(|c| &c.env)
        .find(|var| var.name == branchyard_bridge::KEY_ENV)
        .map(|var| var.value.clone())
}

fn set_state(actor: &mut pb::Actor, state: pb::ActorState) {
    actor.status.get_or_insert_with(Default::default).state = state as i32;
}

fn state_of(actor: &pb::Actor) -> i32 {
    actor.status.as_ref().map_or(0, |s| s.state)
}

fn copy_dir(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

#[derive(Clone)]
struct Service(Arc<Inner>);

#[tonic::async_trait]
impl Control for Service {
    async fn get_actor_template(
        &self,
        request: Request<pb::GetActorTemplateRequest>,
    ) -> Result<Response<pb::ActorTemplate>, Status> {
        let name = self.0.name_of(request.into_inner().actor_template)?;
        let state = self.0.lock();
        let template = state
            .templates
            .get(&name)
            .cloned()
            .ok_or_else(|| Status::not_found(name))?;
        Ok(Response::new(template))
    }

    async fn create_actor(
        &self,
        request: Request<pb::CreateActorRequest>,
    ) -> Result<Response<pb::Actor>, Status> {
        let mut actor = request
            .into_inner()
            .actor
            .ok_or_else(|| Status::invalid_argument("actor"))?;
        let mut state = self.0.lock();
        let metadata = actor
            .metadata
            .as_mut()
            .ok_or_else(|| Status::invalid_argument("metadata"))?;
        if metadata.atespace != self.0.atespace {
            return Err(Status::permission_denied("wrong atespace"));
        }
        if state.actors.contains_key(&metadata.name) {
            return Err(Status::already_exists(metadata.name.clone()));
        }
        let template = actor
            .actor_template
            .as_ref()
            .map(|t| t.name.clone())
            .unwrap_or_default();
        if !state.templates.contains_key(&template) {
            return Err(Status::failed_precondition(format!(
                "unknown template {template}"
            )));
        }
        let seed = match &actor.source_tag {
            Some(tag) => Some(
                state
                    .tags
                    .get(&tag.name)
                    .map(|(_, root)| root.clone())
                    .ok_or_else(|| Status::failed_precondition("unknown tag"))?,
            ),
            None => None,
        };
        state.next_uid += 1;
        metadata.uid = format!("00000000-0000-4000-8000-{:012}", state.next_uid);
        let name = metadata.name.clone();
        let root = self
            .0
            .scratch
            .join("actors")
            .join(format!("{name}-{}", metadata.uid));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).map_err(|e| Status::internal(e.to_string()))?;
        if let Some(seed) = seed {
            copy_dir(&seed, &root).map_err(|e| Status::internal(e.to_string()))?;
        }
        for path in self
            .0
            .fresh
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            let _ = fs::remove_dir_all(path);
        }
        set_state(&mut actor, pb::ActorState::Suspended);
        state.actors.insert(
            name,
            Actor {
                proto: actor.clone(),
                root,
                bridge: None,
            },
        );
        Ok(Response::new(actor))
    }

    async fn get_actor(
        &self,
        request: Request<pb::GetActorRequest>,
    ) -> Result<Response<pb::Actor>, Status> {
        let name = self.0.name_of(request.into_inner().actor)?;
        let state = self.0.lock();
        let actor = state
            .actors
            .get(&name)
            .map(|a| a.proto.clone())
            .ok_or_else(|| Status::not_found(name))?;
        Ok(Response::new(actor))
    }

    async fn resume_actor(
        &self,
        request: Request<pb::ResumeActorRequest>,
    ) -> Result<Response<pb::ResumeActorResponse>, Status> {
        let inner = self.0.clone();
        let reference = request.into_inner().actor;
        let actor = tokio::task::spawn_blocking(move || {
            inner.transition(reference, pb::ActorState::Running)
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(pb::ResumeActorResponse {
            actor: Some(actor),
            resumed: true,
        }))
    }

    async fn suspend_actor(
        &self,
        request: Request<pb::SuspendActorRequest>,
    ) -> Result<Response<pb::SuspendActorResponse>, Status> {
        let inner = self.0.clone();
        let reference = request.into_inner().actor;
        let actor = tokio::task::spawn_blocking(move || {
            inner.transition(reference, pb::ActorState::Suspended)
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(pb::SuspendActorResponse {
            actor: Some(actor),
        }))
    }

    async fn revert_actor(
        &self,
        request: Request<pb::RevertActorRequest>,
    ) -> Result<Response<pb::RevertActorResponse>, Status> {
        let inner = self.0.clone();
        let reference = request.into_inner().actor;
        let actor = tokio::task::spawn_blocking(move || {
            inner.transition(reference, pb::ActorState::Suspended)
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(pb::RevertActorResponse {
            actor: Some(actor),
        }))
    }

    async fn create_tag(
        &self,
        request: Request<pb::CreateTagRequest>,
    ) -> Result<Response<pb::Tag>, Status> {
        let mut tag = request
            .into_inner()
            .tag
            .ok_or_else(|| Status::invalid_argument("tag"))?;
        if tag.scope != pb::TagScope::Atespace as i32 {
            return Err(Status::permission_denied(
                "fake only accepts atespace-scoped tags",
            ));
        }
        let source = self.0.name_of(tag.source_actor.clone())?;
        let mut state = self.0.lock();
        let actor = state
            .actors
            .get(&source)
            .ok_or_else(|| Status::not_found(source.clone()))?;
        if state_of(&actor.proto) != pb::ActorState::Suspended as i32 {
            return Err(Status::failed_precondition(
                "source actor must be suspended",
            ));
        }
        let from = actor.root.clone();
        let metadata = tag
            .metadata
            .as_mut()
            .ok_or_else(|| Status::invalid_argument("metadata"))?;
        if state.tags.contains_key(&metadata.name) {
            return Err(Status::already_exists(metadata.name.clone()));
        }
        metadata.uid = format!("tag-{}", metadata.name);
        let copy = self.0.scratch.join("tags").join(&metadata.name);
        let _ = fs::remove_dir_all(&copy);
        copy_dir(&from, &copy).map_err(|e| Status::internal(e.to_string()))?;
        tag.status = Some(pb::TagStatus {
            snapshot: Some(pb::ExternalSnapshot {
                snapshot_uri: format!("gs://bucket/{source}/{}", metadata.name),
                ..Default::default()
            }),
            ..Default::default()
        });
        state
            .tags
            .insert(metadata.name.clone(), (tag.clone(), copy));
        Ok(Response::new(tag))
    }

    async fn get_tag(
        &self,
        request: Request<pb::GetTagRequest>,
    ) -> Result<Response<pb::Tag>, Status> {
        let name = self.0.name_of(request.into_inner().tag)?;
        let state = self.0.lock();
        let tag = state
            .tags
            .get(&name)
            .map(|(tag, _)| tag.clone())
            .ok_or_else(|| Status::not_found(name))?;
        Ok(Response::new(tag))
    }

    async fn delete_actor(
        &self,
        request: Request<pb::DeleteActorRequest>,
    ) -> Result<Response<pb::Actor>, Status> {
        let request = request.into_inner();
        let name = self.0.name_of(request.actor)?;
        let removed = {
            let mut state = self.0.lock();
            let actor = state
                .actors
                .get(&name)
                .ok_or_else(|| Status::not_found(name.clone()))?;
            let uid = request.options.map(|o| o.uid).unwrap_or_default();
            let actual = actor.proto.metadata.as_ref().map(|m| m.uid.as_str());
            if !uid.is_empty() && actual != Some(uid.as_str()) {
                return Err(Status::failed_precondition("uid precondition failed"));
            }
            if !request.any_state && state_of(&actor.proto) == pb::ActorState::Running as i32 {
                return Err(Status::failed_precondition("actor is running"));
            }
            state.actors.remove(&name).expect("checked above")
        };
        let Actor {
            proto,
            root,
            bridge,
        } = removed;
        tokio::task::spawn_blocking(move || {
            if let Some(bridge) = bridge {
                bridge.stop();
            }
            let _ = fs::remove_dir_all(root);
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(proto))
    }
}

/// Forward one router connection to the actor's bridge.
fn route(inner: &Inner, mut client: TcpStream) -> io::Result<()> {
    let head = branchyard_bridge::ws::read_head(&mut client)?;
    let mut start = head.start.split_whitespace();
    let (method, path) = (start.next().unwrap_or(""), start.next().unwrap_or(""));
    let segments: Vec<&str> = path.splitn(5, '/').collect();
    let (atespace, actor, rest) = match segments.as_slice() {
        ["", "actors", atespace, actor, rest] => (*atespace, *actor, *rest),
        ["", "actors", atespace, actor] => (*atespace, *actor, ""),
        _ => return branchyard_bridge::ws::respond(&mut client, "404 Not Found", "no route\n"),
    };
    let address = {
        let state = inner.lock();
        match state.actors.get(actor) {
            Some(found) if atespace == inner.atespace => found.bridge.as_ref().map(|b| b.address),
            _ => {
                drop(state);
                return branchyard_bridge::ws::respond(
                    &mut client,
                    "404 Not Found",
                    "no such actor\n",
                );
            }
        }
    };
    let Some(address) = address else {
        return branchyard_bridge::ws::respond(
            &mut client,
            "503 Service Unavailable",
            "the actor is not running\n",
        );
    };
    let mut upstream = TcpStream::connect(address)?;
    let mut forwarded = format!("{method} /{rest} HTTP/1.1\r\n");
    for (name, value) in &head.headers {
        forwarded.push_str(&format!("{name}: {value}\r\n"));
    }
    forwarded.push_str("\r\n");
    upstream.write_all(forwarded.as_bytes())?;
    let (mut client_read, mut upstream_write) = (client.try_clone()?, upstream.try_clone()?);
    let up = thread::spawn(move || {
        let _ = io::copy(&mut client_read, &mut upstream_write);
        let _ = upstream_write.shutdown(Shutdown::Write);
    });
    let _ = io::copy(&mut upstream, &mut client);
    let _ = client.shutdown(Shutdown::Write);
    let _ = up.join();
    Ok(())
}

/// A running fake cluster. Dropping it stops every bridge.
pub struct FakeCluster {
    inner: Arc<Inner>,
    runtime: Option<Runtime>,
    endpoint: String,
    router: String,
}

impl FakeCluster {
    /// Serve the fake for `atespace`, with actors that run `bridge` (the
    /// `branchyard-bridge` binary), keeping per-actor state under `scratch`.
    pub fn start(atespace: &str, bridge: Option<PathBuf>, scratch: &Path) -> FakeCluster {
        fs::create_dir_all(scratch).expect("create the fake's scratch directory");
        let inner = Arc::new(Inner {
            atespace: atespace.to_owned(),
            state: Mutex::new(State::default()),
            bridge,
            scratch: scratch.to_path_buf(),
            fresh: Mutex::new(Vec::new()),
        });
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("fake-substrate")
            .enable_all()
            .build()
            .expect("start the fake's runtime");
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .expect("bind the fake control API");
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let service = ControlServer::new(Service(inner.clone()));
        runtime.spawn(async move {
            let _ = Server::builder()
                .add_service(service)
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await;
        });
        let router = TcpListener::bind("127.0.0.1:0").expect("bind the fake router");
        let router_url = format!(
            "http://{}/actors/{{atespace}}/{{actor}}/",
            router.local_addr().unwrap()
        );
        {
            let inner = inner.clone();
            thread::spawn(move || {
                for client in router.incoming().flatten() {
                    let inner = inner.clone();
                    thread::spawn(move || {
                        let _ = route(&inner, client);
                    });
                }
            });
        }
        FakeCluster {
            inner,
            runtime: Some(runtime),
            endpoint,
            router: router_url,
        }
    }

    /// The `Control` API endpoint.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The router URL template, for [`crate::Config::router`].
    pub fn router(&self) -> &str {
        &self.router
    }

    pub fn add_template(&self, template: pb::ActorTemplate) {
        let name = template
            .metadata
            .as_ref()
            .map(|m| m.name.clone())
            .unwrap_or_default();
        self.inner.lock().templates.insert(name, template);
    }

    /// Remove `path` whenever an actor is created, as a fresh root
    /// filesystem would not have it.
    pub fn fresh_on_create(&self, path: impl Into<PathBuf>) {
        self.inner
            .fresh
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(path.into());
    }

    pub fn actor(&self, name: &str) -> Option<pb::Actor> {
        self.inner.lock().actors.get(name).map(|a| a.proto.clone())
    }

    pub fn actor_names(&self) -> Vec<String> {
        let mut names: Vec<_> = self.inner.lock().actors.keys().cloned().collect();
        names.sort();
        names
    }

    /// The process ID of the actor's running bridge.
    pub fn bridge_pid(&self, name: &str) -> Option<u32> {
        let state = self.inner.lock();
        state
            .actors
            .get(name)?
            .bridge
            .as_ref()
            .map(|b| b.child.id())
    }
}

impl Drop for FakeCluster {
    fn drop(&mut self) {
        let bridges: Vec<Bridge> = self
            .inner
            .lock()
            .actors
            .values_mut()
            .filter_map(|a| a.bridge.take())
            .collect();
        for bridge in bridges {
            bridge.stop();
        }
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}
