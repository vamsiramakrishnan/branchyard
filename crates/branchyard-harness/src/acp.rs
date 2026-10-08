//! Agent Client Protocol v1 client, for every ACP profile.
//!
//! Incoming messages are parsed with the official
//! `agent-client-protocol-schema` types. The client advertises no filesystem
//! or terminal capabilities, so an agent must use its own tools inside the
//! sandbox; client requests for those methods are answered with an error.
//!
//! MCP servers are sent as stdio servers in `mcpServers` on `session/new`,
//! `session/resume` and `session/load`, which every ACP agent must accept.
//! ACP has no field for standing instructions, so the first prompt of each
//! opened session starts with them between [`PREAMBLE_OPEN`] and
//! [`PREAMBLE_CLOSE`]; the prompt the caller recorded is unchanged.
//!
//! Resume uses `session/resume` when the agent advertises it, else
//! `session/load`; updates that `session/load` replays are not reported as a
//! new turn. Fork is rejected: `session/fork` is an unstable ACP method, and a
//! driver that cannot fork must say so rather than start a fresh session.
//! Cancellation is the `session/cancel` notification; per the protocol, the
//! client also answers every outstanding permission request as cancelled. The
//! turn's terminal state arrives as the prompt response's `cancelled` stop
//! reason, not as an acknowledgment.
//!
//! ACP v1 has no method for input into a running prompt; a second
//! `session/prompt` is a new turn, which agents queue or refuse. Steering
//! uses the `_session/steering` extension request, only when the agent's
//! `initialize` response advertises `_meta.steering.supported`, and always
//! with `_meta.steering.idleBehavior: "promptRequired"`, so input that
//! arrives after the prompt settled is returned undelivered
//! ([`Event::SteerRejected`]) rather than started as a turn no one tracks.
//! claude-agent-acp 0.81.2 advertises it; checked against a local stand-in
//! API with no model call, it answers `injected`
//! ([`Event::SteerAccepted`]), aborts the model response in progress, keeps
//! its partial text, and continues the same prompt with the steered
//! message, whose one response covers both; a `session/cancel` afterwards
//! ends the prompt `cancelled` with no further model call. An agent that
//! does not advertise the extension refuses steering with the reason.

use std::collections::HashMap;

use agent_client_protocol_schema::v1::{
    ContentBlock, InitializeResponse, PermissionOption, PermissionOptionKind, PromptResponse,
    RequestPermissionRequest, SessionNotification, SessionUpdate, StopReason,
};
use serde_json::json;

use crate::{
    frame, parse, rpc_error, Capabilities, Driver, Event, Frame, LaunchSpec, McpServer,
    NativeSession, Open, Opened, Output, PermissionDecision, PermissionKey, PermissionRequest,
    Rejected, RemoteMcpServer, RemoteTransport, SessionMode, Submitted, TurnOutcome, Turns, Value,
    PREAMBLE_CLOSE, PREAMBLE_OPEN,
};

/// The ACP protocol version this client speaks.
const PROTOCOL_VERSION: u64 = 1;

#[derive(Debug)]
enum Pending {
    Initialize,
    Session,
    Prompt(u64),
    /// A `_session/steering` request: the turn and the steer's number.
    Steer(u64, u64),
}

/// The ACP extension request that injects input into a running prompt.
const STEER_METHOD: &str = "_session/steering";

/// ACP stdio servers: `{name, command, args, env: [{name, value}]}` with no
/// `type`, the untagged `McpServer::Stdio` variant every agent must accept;
/// then HTTP and SSE servers, `{type, name, url, headers: [{name, value}]}`,
/// which an agent accepts only when it advertises `mcpCapabilities`.
fn mcp_servers(servers: &[McpServer], remote: &[RemoteMcpServer]) -> Value {
    let pairs = |pairs: &[(String, String)]| -> Vec<Value> {
        pairs
            .iter()
            .map(|(name, value)| json!({"name": name, "value": value}))
            .collect()
    };
    servers
        .iter()
        .map(|server| {
            json!({"name": server.name, "command": server.command, "args": server.args,
                   "env": pairs(&server.env)})
        })
        .chain(remote.iter().map(|server| {
            json!({"type": server.transport.as_str(), "name": server.name, "url": server.url,
                   "headers": pairs(&server.headers)})
        }))
        .collect()
}

/// An ACP agent session.
#[derive(Debug)]
pub struct Acp {
    command: Vec<String>,
    open: Option<Open>,
    ready: bool,
    /// True while `session/load` replays history.
    loading: bool,
    session: Option<NativeSession>,
    next_id: u64,
    pending: HashMap<u64, Pending>,
    permissions: HashMap<String, (Value, Vec<PermissionOption>)>,
    turns: Turns,
    /// Agent-specific `_meta` sent when opening a session.
    session_meta: Option<Value>,
    /// Instructions to put before the next prompt, once.
    preamble: Option<String>,
    /// The agent advertised `_meta.steering.supported`.
    steering: bool,
    next_steer: u64,
}

impl Acp {
    /// `command` is the full ACP launch, such as `["gemini", "--experimental-acp"]`.
    pub fn new(command: Vec<String>) -> Self {
        Self {
            command,
            open: None,
            ready: false,
            loading: false,
            session: None,
            next_id: 0,
            pending: HashMap::new(),
            permissions: HashMap::new(),
            turns: Turns::default(),
            session_meta: None,
            preamble: None,
            steering: false,
            next_steer: 0,
        }
    }

    /// Send `meta` as `_meta` on `session/new`, `session/resume` and
    /// `session/load`, for agent-specific session options a profile pins.
    pub fn with_session_meta(mut self, meta: Value) -> Self {
        self.session_meta = Some(meta);
        self
    }

    fn request(&mut self, pending: Pending, method: &str, params: Value) -> Frame {
        self.next_id += 1;
        self.pending.insert(self.next_id, pending);
        frame(&json!({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params}))
    }

    #[allow(clippy::expect_used)] // ratchet: branchyard-harness
    fn initialized(&mut self, result: &Value) -> Output {
        let response: InitializeResponse = match serde_json::from_value(result.clone()) {
            Ok(response) => response,
            Err(error) => {
                return Output::event(Event::OpenFailed {
                    reason: format!("unreadable initialize response: {error}"),
                })
            }
        };
        if serde_json::to_value(response.protocol_version).ok() != Some(json!(PROTOCOL_VERSION)) {
            return Output::event(Event::OpenFailed {
                reason: format!(
                    "agent speaks ACP {:?}, not {PROTOCOL_VERSION}",
                    response.protocol_version
                ),
            });
        }
        let open = self.open.clone().expect("initialize follows open()");
        // A top-level `_meta` extension, beside `agentCapabilities`.
        self.steering = result["_meta"]["steering"]["supported"] == true;
        let capabilities = response.agent_capabilities;
        let accepted = |transport: RemoteTransport| match transport {
            RemoteTransport::Http => capabilities.mcp_capabilities.http,
            RemoteTransport::Sse => capabilities.mcp_capabilities.sse,
        };
        if let Some(server) = open
            .remote_mcp_servers
            .iter()
            .find(|s| !accepted(s.transport))
        {
            return Output::event(Event::OpenFailed {
                reason: format!(
                    "the agent does not accept {} MCP servers ({})",
                    server.transport.as_str().to_uppercase(),
                    server.name
                ),
            });
        }
        let servers = mcp_servers(&open.mcp_servers, &open.remote_mcp_servers);
        let (method, mut params) = match &open.mode {
            SessionMode::Fresh => (
                "session/new",
                json!({"cwd": open.cwd, "mcpServers": servers}),
            ),
            SessionMode::Resume(session) if capabilities.session_capabilities.resume.is_some() => (
                "session/resume",
                json!({"sessionId": session.as_str(), "cwd": open.cwd, "mcpServers": servers}),
            ),
            SessionMode::Resume(session) if capabilities.load_session => {
                self.loading = true;
                (
                    "session/load",
                    json!({"sessionId": session.as_str(), "cwd": open.cwd, "mcpServers": servers}),
                )
            }
            SessionMode::Resume(_) => {
                return Output::event(Event::OpenFailed {
                    reason: "agent advertises neither session/resume nor session/load".into(),
                })
            }
            SessionMode::Fork(_) => unreachable!("fork is rejected in open()"),
        };
        if let Some(meta) = &self.session_meta {
            params["_meta"] = meta.clone();
        }
        Output {
            events: Vec::new(),
            frames: vec![self.request(Pending::Session, method, params)],
        }
    }

    fn session_opened(&mut self, result: &Value) -> Output {
        self.loading = false;
        let mode = self.open.as_ref().map(|open| open.mode.clone());
        let session = match mode {
            Some(SessionMode::Resume(session)) => session,
            _ => match result["sessionId"].as_str().and_then(NativeSession::new) {
                Some(session) => session,
                None => {
                    return Output::event(Event::OpenFailed {
                        reason: "session/new response without a usable sessionId".into(),
                    })
                }
            },
        };
        self.session = Some(session.clone());
        self.ready = true;
        Output {
            events: vec![
                Event::Ready,
                Event::SessionStarted {
                    session,
                    forked_from: None,
                },
            ],
            frames: Vec::new(),
        }
    }

    fn response(&mut self, message: &Value) -> Output {
        let Some(pending) = message["id"]
            .as_u64()
            .and_then(|id| self.pending.remove(&id))
        else {
            return Output::event(Event::ProtocolViolation {
                detail: format!("response to unknown request {}", message["id"]),
            });
        };
        let error = message.get("error").map(rpc_error);
        match (pending, error) {
            (Pending::Initialize, None) => self.initialized(&message["result"]),
            (Pending::Session, None) => self.session_opened(&message["result"]),
            (Pending::Initialize | Pending::Session, Some(error)) => {
                self.loading = false;
                Output::event(Event::OpenFailed { reason: error })
            }
            (Pending::Steer(turn, steer), Some(reason)) => Output::event(Event::SteerRejected {
                turn,
                steer,
                reason,
            }),
            (Pending::Steer(turn, steer), None) => match message["result"]["outcome"].as_str() {
                Some("injected") => Output::event(Event::SteerAccepted { turn, steer }),
                Some("promptRequired") => Output::event(Event::SteerRejected {
                    turn,
                    steer,
                    reason: "the agent's prompt had already ended".into(),
                }),
                other => Output::event(Event::SteerRejected {
                    turn,
                    steer,
                    reason: format!("the agent answered with outcome {other:?}"),
                }),
            },
            (Pending::Prompt(turn), error) => {
                self.turns.end();
                self.permissions.clear();
                let outcome = match error {
                    Some(message) => TurnOutcome::Failed { message },
                    None => {
                        match serde_json::from_value::<PromptResponse>(message["result"].clone()) {
                            Ok(response) => stop_outcome(response.stop_reason),
                            Err(error) => TurnOutcome::Failed {
                                message: format!("unreadable prompt response: {error}"),
                            },
                        }
                    }
                };
                Output::event(Event::TurnEnded { turn, outcome })
            }
        }
    }

    fn update(&mut self, params: &Value) -> Output {
        let notification: SessionNotification = match serde_json::from_value(params.clone()) {
            Ok(notification) => notification,
            Err(_) => {
                let kind = params["update"]["sessionUpdate"]
                    .as_str()
                    .unwrap_or("unknown");
                return Output::event(Event::Unrecognized {
                    kind: format!("session/update:{kind}"),
                });
            }
        };
        // Updates must belong to the open session; while loading, that is
        // the session being resumed.
        let expected = self
            .session
            .clone()
            .or_else(|| match self.open.as_ref().map(|o| &o.mode) {
                Some(SessionMode::Resume(session)) => Some(session.clone()),
                _ => None,
            });
        let actual = notification.session_id.to_string();
        if let Some(expected) = expected {
            if actual != expected.as_str() {
                return Output::event(Event::ProtocolViolation {
                    detail: format!("session/update for session {actual}, not {expected}"),
                });
            }
        }
        // History replayed by session/load belongs to earlier turns.
        let Some(turn) = self.turns.active.filter(|_| !self.loading) else {
            return Output::default();
        };
        match notification.update {
            SessionUpdate::AgentMessageChunk(chunk) => match chunk.content {
                ContentBlock::Text(text) => Output::event(Event::MessageDelta {
                    turn,
                    text: text.text,
                }),
                _ => Output::default(),
            },
            SessionUpdate::ToolCall(call) => Output::event(Event::ToolStarted {
                turn,
                call_id: call.tool_call_id.to_string(),
                name: call.name.unwrap_or(call.title),
            }),
            _ => Output::default(),
        }
    }

    fn client_request(&mut self, message: &Value, method: &str) -> Output {
        let id = message["id"].clone();
        if method == "session/request_permission" {
            if let Ok(request) =
                serde_json::from_value::<RequestPermissionRequest>(message["params"].clone())
            {
                let fields = &request.tool_call.fields;
                let tool = fields
                    .title
                    .clone()
                    .unwrap_or_else(|| request.tool_call.tool_call_id.to_string());
                let input = fields
                    .raw_input
                    .clone()
                    .unwrap_or_else(|| message["params"]["toolCall"].clone());
                let key = id.to_string();
                self.permissions.insert(key.clone(), (id, request.options));
                return Output::event(Event::PermissionRequested {
                    turn: self.turns.active,
                    request: PermissionRequest {
                        key: PermissionKey(key),
                        tool,
                        input,
                    },
                });
            }
        }
        let reply = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": format!("branchyard does not handle {method}")},
        });
        Output {
            events: vec![Event::UnsupportedRequest {
                method: method.to_owned(),
            }],
            frames: vec![frame(&reply)],
        }
    }
}

fn stop_outcome(reason: StopReason) -> TurnOutcome {
    match reason {
        StopReason::EndTurn => TurnOutcome::Completed,
        StopReason::Cancelled => TurnOutcome::Interrupted,
        StopReason::Refusal => TurnOutcome::Refused,
        StopReason::MaxTokens => TurnOutcome::LimitReached {
            limit: "max_tokens".into(),
        },
        StopReason::MaxTurnRequests => TurnOutcome::LimitReached {
            limit: "max_turn_requests".into(),
        },
        #[allow(unreachable_patterns)]
        other => TurnOutcome::Failed {
            message: format!("unrecognized stop reason {other:?}"),
        },
    }
}

impl Driver for Acp {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // Enforced again against the agent's advertised capabilities.
            resume: true,
            fork: false,
            cancellation: true,
            tool_approvals: true,
            turn_acknowledgment: false,
            usage: false,
            // Enforced again against the agent's `_meta.steering`.
            steer: true,
            model: false,
        }
    }

    fn capability_reasons(&self) -> &'static [crate::CapabilityReason] {
        &[
            (
                "resume",
                "uses session/resume when the agent advertises it, else session/load; a session that cannot resume fails to open rather than starting fresh",
            ),
            (
                "fork",
                "ACP session/fork is an unstable method; a driver that cannot fork must say so rather than start a fresh session",
            ),
            ("turn_acknowledgment", "not verified"),
            (
                "model",
                "ACP v1 has no model parameter; selection belongs to the agent's own configuration",
            ),
            ("usage", "not verified"),
        ]
    }

    fn open(&mut self, open: Open) -> Result<Opened, Rejected> {
        if open.cwd.is_empty() {
            return Err(Rejected::InvalidOpen("empty working directory".into()));
        }
        if self.open.is_some() {
            return Err(Rejected::InvalidOpen("the session is already open".into()));
        }
        if matches!(open.mode, SessionMode::Fork(_)) {
            return Err(Rejected::Unsupported(
                "ACP session/fork is an unstable method".into(),
            ));
        }
        if open.model.is_some() {
            // ACP v1 has no model parameter; selection belongs to the profile.
            return Err(Rejected::Unsupported("model selection over ACP".into()));
        }
        crate::check_all_mcp_servers(&open)?;
        self.preamble = open.instructions.as_ref().map(|i| i.text.clone());
        let launch = LaunchSpec {
            argv: self.command.clone(),
            cwd: open.cwd.clone(),
        };
        self.open = Some(open);
        let initialize = self.request(
            Pending::Initialize,
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
                "clientInfo": {"name": "branchyard", "version": env!("CARGO_PKG_VERSION")},
            }),
        );
        Ok(Opened {
            launch,
            frames: vec![initialize],
        })
    }

    fn receive(&mut self, line: &[u8]) -> Output {
        let message = match parse(line) {
            Ok(message) => message,
            Err(output) => return output,
        };
        if message["jsonrpc"] != "2.0" {
            return Output::event(Event::ProtocolViolation {
                detail: "message without jsonrpc 2.0".into(),
            });
        }
        match (message["method"].as_str(), message.get("id").is_some()) {
            (Some(method), true) => self.client_request(&message, method),
            (Some("session/update"), false) => self.update(&message["params"]),
            (Some(method), false) => Output::event(Event::Unrecognized {
                kind: method.to_owned(),
            }),
            (None, true) => self.response(&message),
            (None, false) => Output::event(Event::ProtocolViolation {
                detail: "message with neither method nor id".into(),
            }),
        }
    }

    fn submit(&mut self, prompt: &str) -> Result<Submitted, Rejected> {
        let session = match (&self.session, self.ready) {
            (Some(session), true) => session.to_string(),
            _ => return Err(Rejected::NotReady),
        };
        let turn = self.turns.begin()?;
        let text = match self.preamble.take() {
            Some(preamble) => {
                format!("{PREAMBLE_OPEN}\n{preamble}\n{PREAMBLE_CLOSE}\n\n{prompt}")
            }
            None => prompt.to_owned(),
        };
        let request = self.request(
            Pending::Prompt(turn),
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]}),
        );
        Ok(Submitted {
            turn,
            frames: vec![request],
        })
    }

    fn interrupt(&mut self) -> Result<Vec<Frame>, Rejected> {
        self.turns.active.ok_or(Rejected::NoTurn)?;
        let session = self.session.as_ref().ok_or(Rejected::NotReady)?.to_string();
        let mut frames = vec![frame(&json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": {"sessionId": session},
        }))];
        for (_, (id, _)) in self.permissions.drain() {
            frames.push(frame(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"outcome": {"outcome": "cancelled"}},
            })));
        }
        Ok(frames)
    }

    fn steer_boundary(&self) -> &'static str {
        // The `_session/steering` extension request, only when advertised
        // (`docs/harness-integration.md` "Steering a running turn").
        "acp_session_steering"
    }

    fn steer(&mut self, text: &str) -> Result<Vec<Frame>, Rejected> {
        let session = match (&self.session, self.ready) {
            (Some(session), true) => session.to_string(),
            _ => return Err(Rejected::NotReady),
        };
        let turn = self.turns.active.ok_or(Rejected::NoTurn)?;
        if !self.steering {
            return Err(Rejected::Unsupported(format!(
                "the agent does not advertise the {STEER_METHOD} extension"
            )));
        }
        self.next_steer += 1;
        let steer = self.next_steer;
        Ok(vec![self.request(
            Pending::Steer(turn, steer),
            STEER_METHOD,
            json!({
                "sessionId": session,
                "prompt": [{"type": "text", "text": text}],
                "_meta": {"steering": {"idleBehavior": "promptRequired"}},
            }),
        )])
    }

    #[allow(clippy::expect_used)] // ratchet: branchyard-harness
    fn respond(
        &mut self,
        key: &PermissionKey,
        decision: PermissionDecision,
    ) -> Result<Vec<Frame>, Rejected> {
        let (_, options) = self
            .permissions
            .get(&key.0)
            .ok_or(Rejected::UnknownPermission)?;
        // Only one-time options: a decision about one invocation must not
        // become a standing rule.
        let wanted = match decision {
            PermissionDecision::Allow => PermissionOptionKind::AllowOnce,
            PermissionDecision::Deny { .. } => PermissionOptionKind::RejectOnce,
        };
        let option = options
            .iter()
            .find(|option| option.kind == wanted)
            .ok_or_else(|| Rejected::Unsupported(format!("agent offered no {wanted:?} option")))?
            .option_id
            .to_string();
        let (id, _) = self.permissions.remove(&key.0).expect("checked above");
        Ok(vec![frame(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"outcome": {"outcome": "selected", "optionId": option}},
        }))])
    }

    fn transport_closed(&mut self) -> Vec<Event> {
        self.ready = false;
        self.loading = false;
        self.steering = false;
        self.pending.clear();
        self.permissions.clear();
        self.turns.closed()
    }
}
