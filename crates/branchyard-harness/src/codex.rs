//! Codex App Server over JSON-RPC on stdio.
//!
//! Launches `codex app-server`. Frames follow the schema that
//! `codex app-server generate-json-schema` emits for codex-cli 0.157.1:
//! JSON-RPC 2.0 messages without the `jsonrpc` member, an `initialize` /
//! `initialized` handshake, `thread/start`, `thread/resume` or `thread/fork`,
//! then `turn/start` and `turn/interrupt`.
//!
//! MCP servers travel in the thread request's `config`, which the schema
//! types as an open object of configuration overrides (`ThreadStartParams`,
//! `ThreadResumeParams` and `ThreadForkParams` all carry it), as
//! `{"mcp_servers": {<name>: {command, args, env}}}`, the shape of the
//! `[mcp_servers.<name>]` tables in `config.toml`. codex-cli 0.157.1 started
//! and connected such a server on `thread/start` when checked; resume and
//! fork send the same override but were not checked live.
//!
//! Instructions go in `developerInstructions`, a string field of all three
//! thread requests in the same schema.
//!
//! Threads start with `approvalPolicy: "on-request"` so command and file
//! change approvals reach Branchyard as server requests, and Codex's own
//! sandbox in `workspace-write` mode inside Branchyard's sandbox. Server
//! requests this profile does not implement are answered with a JSON-RPC
//! error rather than left pending.
//!
//! Steering is `turn/steer`, naming the turn in flight as
//! `expectedTurnId`, so input meant for one turn never lands in another;
//! until `turn/start` has answered with the turn's ID, a steer is
//! [`Rejected::SteerNotYet`]. codex-cli 0.157.1, checked against a local
//! stand-in Responses API with no model call, answers with the turn's ID
//! ([`Event::SteerAccepted`]) and, once the model response in progress
//! finishes, records the input as a `userMessage` item carrying the
//! `clientUserMessageId` the driver sent, then samples again within the
//! same turn: one `turn/completed` covers both. A steer for another turn,
//! or with none active, is a JSON-RPC error ([`Event::SteerRejected`]).
//! An interrupt drops accepted input not yet recorded; the driver reports
//! each such steer as rejected when the turn completes.

use std::collections::HashMap;

use serde_json::json;

use crate::{
    frame, parse, rpc_error, Capabilities, Driver, Event, Frame, LaunchSpec, McpServer,
    NativeSession, Open, Opened, Output, PermissionDecision, PermissionKey, PermissionRequest,
    Rejected, SessionMode, Submitted, TurnOutcome, Turns, Usage, Value,
};

#[derive(Debug)]
enum Pending {
    Initialize,
    Thread,
    TurnStart(u64),
    Interrupt(u64),
    /// A `turn/steer`: the turn and the steer's number.
    Steer(u64, u64),
}

/// Approval requests this profile answers.
const APPROVALS: [(&str, &str); 2] = [
    ("item/commandExecution/requestApproval", "commandExecution"),
    ("item/fileChange/requestApproval", "fileChange"),
];

/// Item types reported as tool starts.
const TOOL_ITEMS: [&str; 6] = [
    "commandExecution",
    "fileChange",
    "mcpToolCall",
    "dynamicToolCall",
    "webSearch",
    "imageGeneration",
];

/// `mcp_servers` as a thread `config` override: the `[mcp_servers.<name>]`
/// tables of Codex's `config.toml`, with `command`, `args` and `env`.
fn mcp_servers(servers: &[McpServer]) -> Value {
    let servers: serde_json::Map<String, Value> = servers
        .iter()
        .map(|server| {
            let env: serde_json::Map<String, Value> = server
                .env
                .iter()
                .map(|(name, value)| (name.clone(), json!(value)))
                .collect();
            let table = json!({"command": server.command, "args": server.args, "env": env});
            (server.name.clone(), table)
        })
        .collect();
    Value::Object(servers)
}

/// A Codex App Server session.
#[derive(Debug)]
pub struct Codex {
    command: Vec<String>,
    open: Option<Open>,
    ready: bool,
    thread: Option<NativeSession>,
    next_id: u64,
    pending: HashMap<u64, Pending>,
    /// Outstanding approvals: key to the JSON-RPC request ID to answer.
    approvals: HashMap<String, Value>,
    turns: Turns,
    native_turn: Option<String>,
    /// Steers of the turn in flight that Codex has not yet recorded as a
    /// user message: number and client message ID.
    steers: Vec<(u64, String)>,
    /// Steers already reported rejected when their turn ended, whose
    /// answer may still arrive.
    dropped: Vec<u64>,
    next_steer: u64,
}

impl Codex {
    /// `command` is the executable and any fixed leading arguments, usually
    /// `["codex"]`.
    pub fn new(command: Vec<String>) -> Self {
        Self {
            command,
            open: None,
            ready: false,
            thread: None,
            next_id: 0,
            pending: HashMap::new(),
            approvals: HashMap::new(),
            turns: Turns::default(),
            native_turn: None,
            steers: Vec::new(),
            dropped: Vec::new(),
            next_steer: 0,
        }
    }

    fn request(&mut self, pending: Pending, method: &str, params: Value) -> Frame {
        self.next_id += 1;
        self.pending.insert(self.next_id, pending);
        frame(&json!({"id": self.next_id, "method": method, "params": params}))
    }

    fn thread_request(&mut self) -> Frame {
        let open = self.open.clone().expect("the thread opens after open()");
        let mut params = json!({
            "cwd": open.cwd,
            "approvalPolicy": "on-request",
            "sandbox": "workspace-write",
        });
        if let Some(model) = open.model {
            params["model"] = json!(model);
        }
        if !open.mcp_servers.is_empty() {
            params["config"] = json!({ "mcp_servers": mcp_servers(&open.mcp_servers) });
        }
        if let Some(instructions) = &open.instructions {
            params["developerInstructions"] = json!(instructions.text);
        }
        let method = match &open.mode {
            SessionMode::Fresh => "thread/start",
            SessionMode::Resume(thread) => {
                params["threadId"] = json!(thread.as_str());
                "thread/resume"
            }
            SessionMode::Fork(thread) => {
                params["threadId"] = json!(thread.as_str());
                "thread/fork"
            }
        };
        self.request(Pending::Thread, method, params)
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
            (Pending::Initialize, None) => Output {
                events: Vec::new(),
                frames: vec![
                    frame(&json!({"method": "initialized"})),
                    self.thread_request(),
                ],
            },
            (Pending::Initialize | Pending::Thread, Some(error)) => {
                Output::event(Event::OpenFailed { reason: error })
            }
            (Pending::Thread, None) => self.thread_opened(&message["result"]["thread"]),
            (Pending::TurnStart(turn), None) => {
                let native = message["result"]["turn"]["id"].as_str().map(str::to_owned);
                if self.native_turn.is_none() {
                    self.native_turn = native.clone();
                }
                Output::event(Event::TurnAccepted { turn, native })
            }
            (Pending::TurnStart(turn), Some(message)) => {
                self.turns.end();
                self.native_turn = None;
                Output::event(Event::TurnEnded {
                    turn,
                    outcome: TurnOutcome::Failed { message },
                })
            }
            (Pending::Interrupt(turn), None) => {
                Output::event(Event::InterruptAcknowledged { turn })
            }
            (Pending::Interrupt(_), Some(error)) => Output::event(Event::Warning {
                message: format!("interrupt failed: {error}"),
            }),
            (Pending::Steer(_, steer), _) if self.dropped.contains(&steer) => Output::default(),
            (Pending::Steer(turn, steer), None) => {
                Output::event(Event::SteerAccepted { turn, steer })
            }
            (Pending::Steer(turn, steer), Some(reason)) => {
                self.steers.retain(|(n, _)| *n != steer);
                Output::event(Event::SteerRejected {
                    turn,
                    steer,
                    reason,
                })
            }
        }
    }

    fn thread_opened(&mut self, thread: &Value) -> Output {
        let Some(id) = thread["id"].as_str().and_then(NativeSession::new) else {
            return Output::event(Event::OpenFailed {
                reason: "thread response without a usable thread ID".into(),
            });
        };
        let mode = self.open.as_ref().map(|open| &open.mode);
        let forked_from = match mode {
            Some(SessionMode::Resume(expected)) if *expected != id => {
                return Output::event(Event::OpenFailed {
                    reason: format!("resume of {expected} opened thread {id} instead"),
                })
            }
            Some(SessionMode::Fork(parent)) if *parent == id => {
                return Output::event(Event::OpenFailed {
                    reason: format!("fork of {parent} kept the parent's thread ID"),
                })
            }
            Some(SessionMode::Fork(parent)) => Some(parent.clone()),
            _ => None,
        };
        self.thread = Some(id.clone());
        self.ready = true;
        Output {
            events: vec![
                Event::Ready,
                Event::SessionStarted {
                    session: id,
                    forked_from,
                },
            ],
            frames: Vec::new(),
        }
    }

    fn server_request(&mut self, message: &Value, method: &str) -> Output {
        let id = message["id"].clone();
        if let Some((_, tool)) = APPROVALS.iter().find(|(m, _)| *m == method) {
            let key = id.to_string();
            self.approvals.insert(key.clone(), id);
            return Output::event(Event::PermissionRequested {
                turn: self.turns.active,
                request: PermissionRequest {
                    key: PermissionKey(key),
                    tool: (*tool).to_owned(),
                    input: message["params"].clone(),
                },
            });
        }
        let reply = json!({
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

    fn notification(&mut self, message: &Value, method: &str) -> Output {
        let params = &message["params"];
        match method {
            "configWarning" | "warning" | "deprecationNotice" => return warning(params),
            "error" => return warning(&params["error"]),
            // Session bookkeeping that the thread response already covers.
            "thread/started" | "thread/status/changed" | "remoteControl/status/changed" => {
                return Output::default()
            }
            _ => {}
        }
        let Some(turn) = self.turns.active else {
            return Output::event(Event::Unrecognized {
                kind: method.to_owned(),
            });
        };
        match method {
            "turn/started" => Output::default(),
            "item/agentMessage/delta" => Output::event(Event::MessageDelta {
                turn,
                text: params["delta"].as_str().unwrap_or_default().to_owned(),
            }),
            "item/started" => {
                let item = &params["item"];
                let kind = item["type"].as_str().unwrap_or_default();
                if let ("userMessage", Some(client)) = (kind, item["clientId"].as_str()) {
                    // A steer Codex has now put into the conversation.
                    self.steers.retain(|(_, id)| id != client);
                }
                if TOOL_ITEMS.contains(&kind) {
                    Output::event(Event::ToolStarted {
                        turn,
                        call_id: item["id"].as_str().unwrap_or_default().to_owned(),
                        name: kind.to_owned(),
                    })
                } else {
                    Output::default()
                }
            }
            "thread/tokenUsage/updated" => {
                let total = &params["tokenUsage"]["total"];
                Output::event(Event::UsageObserved {
                    turn: Some(turn),
                    usage: Usage {
                        cumulative: true,
                        input_tokens: total["inputTokens"].as_u64(),
                        output_tokens: total["outputTokens"].as_u64(),
                        cached_input_tokens: total["cachedInputTokens"].as_u64(),
                        cost_usd: None,
                    },
                })
            }
            "serverRequest/resolved" => {
                let key = params["requestId"].to_string();
                if self.approvals.remove(&key).is_some() {
                    Output::event(Event::PermissionWithdrawn {
                        key: PermissionKey(key),
                    })
                } else {
                    Output::default()
                }
            }
            "turn/completed" => self.turn_completed(turn, params),
            "item/completed" => Output::default(),
            other => Output::event(Event::Unrecognized {
                kind: other.to_owned(),
            }),
        }
    }

    fn turn_completed(&mut self, turn: u64, params: &Value) -> Output {
        let completed = &params["turn"];
        if let (Some(expected), Some(actual)) = (&self.native_turn, completed["id"].as_str()) {
            if expected != actual {
                return Output::event(Event::Unrecognized {
                    kind: "turn/completed for another turn".into(),
                });
            }
        }
        self.turns.end();
        self.native_turn = None;
        self.approvals.clear();
        let mut events: Vec<Event> = std::mem::take(&mut self.steers)
            .into_iter()
            .inspect(|(steer, _)| self.dropped.push(*steer))
            .map(|(steer, _)| Event::SteerRejected {
                turn,
                steer,
                reason: "the turn ended before Codex delivered it".into(),
            })
            .collect();
        let outcome = match completed["status"].as_str() {
            Some("completed") => TurnOutcome::Completed,
            Some("interrupted") => TurnOutcome::Interrupted,
            _ => TurnOutcome::Failed {
                message: completed["error"]["message"]
                    .as_str()
                    .unwrap_or("turn failed")
                    .to_owned(),
            },
        };
        events.push(Event::TurnEnded { turn, outcome });
        Output {
            events,
            frames: Vec::new(),
        }
    }
}

fn warning(params: &Value) -> Output {
    let message = params["message"]
        .as_str()
        .or_else(|| params["summary"].as_str())
        .unwrap_or("warning");
    Output::event(Event::Warning {
        message: message.to_owned(),
    })
}

impl Driver for Codex {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            resume: true,
            fork: true,
            cancellation: true,
            tool_approvals: true,
            turn_acknowledgment: true,
            usage: true,
            steer: true,
        }
    }

    fn open(&mut self, open: Open) -> Result<Opened, Rejected> {
        if open.cwd.is_empty() {
            return Err(Rejected::InvalidOpen("empty working directory".into()));
        }
        if self.open.is_some() {
            return Err(Rejected::InvalidOpen("the session is already open".into()));
        }
        crate::check_mcp_servers(&open.mcp_servers)?;
        // Codex has `url` servers in `config.toml`, but their thread
        // override has not been checked against codex-cli.
        crate::refuse_remote_mcp(&open, "Codex")?;
        let mut argv = self.command.clone();
        argv.push("app-server".into());
        let launch = LaunchSpec {
            argv,
            cwd: open.cwd.clone(),
        };
        self.open = Some(open);
        let initialize = self.request(
            Pending::Initialize,
            "initialize",
            json!({"clientInfo": {"name": "branchyard", "version": env!("CARGO_PKG_VERSION")}}),
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
        match (message["method"].as_str(), message.get("id").is_some()) {
            (Some(method), true) => self.server_request(&message, method),
            (Some(method), false) => self.notification(&message, method),
            (None, true) => self.response(&message),
            (None, false) => Output::event(Event::ProtocolViolation {
                detail: "message with neither method nor id".into(),
            }),
        }
    }

    fn submit(&mut self, prompt: &str) -> Result<Submitted, Rejected> {
        let thread = match (&self.thread, self.ready) {
            (Some(thread), true) => thread.to_string(),
            _ => return Err(Rejected::NotReady),
        };
        let turn = self.turns.begin()?;
        self.steers.clear();
        let request = self.request(
            Pending::TurnStart(turn),
            "turn/start",
            json!({"threadId": thread, "input": [{"type": "text", "text": prompt}]}),
        );
        Ok(Submitted {
            turn,
            frames: vec![request],
        })
    }

    fn interrupt(&mut self) -> Result<Vec<Frame>, Rejected> {
        let turn = self.turns.active.ok_or(Rejected::NoTurn)?;
        // turn/interrupt names the native turn, known once turn/start answers.
        let native = self.native_turn.clone().ok_or(Rejected::NotReady)?;
        let thread = self.thread.as_ref().ok_or(Rejected::NotReady)?.to_string();
        Ok(vec![self.request(
            Pending::Interrupt(turn),
            "turn/interrupt",
            json!({"threadId": thread, "turnId": native}),
        )])
    }

    fn steer(&mut self, text: &str) -> Result<Vec<Frame>, Rejected> {
        let thread = match (&self.thread, self.ready) {
            (Some(thread), true) => thread.to_string(),
            _ => return Err(Rejected::NotReady),
        };
        let turn = self.turns.active.ok_or(Rejected::NoTurn)?;
        // turn/steer names the native turn, known once turn/start answers.
        let native = self.native_turn.clone().ok_or(Rejected::SteerNotYet)?;
        self.next_steer += 1;
        let steer = self.next_steer;
        let client = format!("branchyard-steer-{steer}");
        self.steers.push((steer, client.clone()));
        Ok(vec![self.request(
            Pending::Steer(turn, steer),
            "turn/steer",
            json!({
                "threadId": thread,
                "expectedTurnId": native,
                "clientUserMessageId": client,
                "input": [{"type": "text", "text": text}],
            }),
        )])
    }

    fn respond(
        &mut self,
        key: &PermissionKey,
        decision: PermissionDecision,
    ) -> Result<Vec<Frame>, Rejected> {
        let id = self
            .approvals
            .remove(&key.0)
            .ok_or(Rejected::UnknownPermission)?;
        // Codex decisions carry no message; decline lets the turn continue.
        let decision = match decision {
            PermissionDecision::Allow => "accept",
            PermissionDecision::Deny { .. } => "decline",
        };
        Ok(vec![frame(
            &json!({"id": id, "result": {"decision": decision}}),
        )])
    }

    fn transport_closed(&mut self) -> Vec<Event> {
        self.ready = false;
        self.pending.clear();
        self.approvals.clear();
        self.native_turn = None;
        self.steers.clear();
        self.turns.closed()
    }
}
