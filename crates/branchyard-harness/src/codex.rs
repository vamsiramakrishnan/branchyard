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
//! Threads start with `approvalPolicy: "untrusted"` so every command
//! outside Codex's own trusted set (`ls`, `cat`, `sed -n` and the like) and
//! every file change reach Branchyard as approval requests, and Codex's own
//! sandbox in `workspace-write` mode inside Branchyard's sandbox. Under
//! `on-request`, codex-cli 0.157.1 ran a write inside the workspace (`echo
//! qualified > deny-marker.txt`) without asking, so a policy that denied the
//! shell never got to deny it (`docs/qualification/README.md`, 12 October
//! 2026); `untrusted` asks before it. Server requests this profile does not
//! implement are answered with a JSON-RPC error rather than left pending.
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

use crate::target;

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

/// The tool name an ordinary command approval is decided under.
const COMMAND_EXECUTION: &str = "commandExecution";

/// The tool name a command approval that asks to leave Codex's sandbox is
/// decided under: no preset allows it by name, so only a policy that allows
/// everything, or one that names it, approves it.
pub const ESCALATED_COMMAND: &str = "commandExecution:escalated";

/// The approval policy every thread runs under: Codex asks before each
/// command and patch its sandbox does not already allow.
const APPROVAL_POLICY: &str = "untrusted";

/// Who answers Codex's approval requests: the client, never an automatic
/// reviewer.
const APPROVALS_REVIEWER: &str = "user";

/// Approval requests this profile answers.
const APPROVALS: [(&str, &str); 2] = [
    ("item/commandExecution/requestApproval", COMMAND_EXECUTION),
    ("item/fileChange/requestApproval", "fileChange"),
];

/// Notifications Branchyard shows nothing of, ignored on purpose rather than
/// reported as unrecognized: the model's reasoning as it streams (hundreds
/// of `item/reasoning/textDelta` per turn on a reasoning model), a command's
/// or a patch's output as it streams, Codex's running diff (the candidate
/// has its own), the lifecycle of Codex's own hooks, and the account's rate
/// limits. Recorded against codex-cli 0.164.0 (`tests/fixtures/codex-0.164.0-file-change-approval.jsonl`).
const QUIET: [&str; 9] = [
    "item/reasoning/textDelta",
    "item/reasoning/summaryTextDelta",
    "item/reasoning/summaryPartAdded",
    "item/commandExecution/outputDelta",
    "item/fileChange/outputDelta",
    "turn/diff/updated",
    "hook/started",
    "hook/completed",
    "account/rateLimits/updated",
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
    /// The files of each `fileChange` item in flight, by item ID: Codex's
    /// approval request for a patch names the item, not the files.
    file_changes: HashMap<String, Vec<String>>,
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
            file_changes: HashMap::new(),
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

    #[allow(clippy::expect_used)] // ratchet: branchyard-harness
    fn thread_request(&mut self) -> Frame {
        let open = self.open.clone().expect("the thread opens after open()");
        // `untrusted` asks before every command and patch the sandbox does
        // not already allow; `user` keeps the asking with Branchyard, not an
        // automatic reviewer a configuration names. A permission profile
        // Branchyard prepared sets the sandbox itself: naming one here as
        // well would discard the profile.
        let mut params = json!({
            "cwd": open.cwd,
            "approvalPolicy": APPROVAL_POLICY,
            "approvalsReviewer": APPROVALS_REVIEWER,
        });
        if open.permission_profile.is_none() {
            params["sandbox"] = json!("workspace-write");
        }
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
            // `excludeTurns`: the answer is read for the thread's ID alone, and
            // codex-cli 0.164.0 warns on each resume and fork that hydrating
            // the full history "is deprecated for paginated threads".
            SessionMode::Resume(thread) => {
                params["threadId"] = json!(thread.as_str());
                params["excludeTurns"] = json!(true);
                "thread/resume"
            }
            SessionMode::Fork(thread) => {
                params["threadId"] = json!(thread.as_str());
                params["excludeTurns"] = json!(true);
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
            (Pending::Thread, None) => match self.thread_settings(&message["result"]) {
                Some(reason) => Output::event(Event::OpenFailed { reason }),
                None => self.thread_opened(&message["result"]["thread"]),
            },
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

    /// Why the thread's reported settings could let Codex run a call
    /// Branchyard did not approve, or `None` when they are the ones asked
    /// for. An administrator's `requirements.toml` can replace a requested
    /// approval policy, reviewer or sandbox with a warning and nothing else,
    /// so the answer is read back rather than assumed.
    fn thread_settings(&self, result: &Value) -> Option<String> {
        let reported = |field: &str| result[field].as_str().unwrap_or("(none)").to_owned();
        if result["approvalPolicy"] != APPROVAL_POLICY {
            return Some(format!(
                "Codex runs the thread with approval policy {}, not {APPROVAL_POLICY}, so it \
                 could run commands without asking Branchyard",
                reported("approvalPolicy")
            ));
        }
        if result["approvalsReviewer"] != APPROVALS_REVIEWER {
            return Some(format!(
                "Codex has approvals decided by {}, not the {APPROVALS_REVIEWER}, so they would \
                 not reach Branchyard",
                reported("approvalsReviewer")
            ));
        }
        let profile = self
            .open
            .as_ref()
            .and_then(|open| open.permission_profile.as_deref());
        match profile {
            Some(id) if result["activePermissionProfile"]["id"] != id => Some(format!(
                "Codex did not take Branchyard's permission profile {id:?} (it reports {}), \
                 so an approved command could run outside its sandbox",
                result["activePermissionProfile"]
            )),
            Some(_) => None,
            None if !matches!(
                result["sandbox"]["type"].as_str(),
                Some("workspaceWrite" | "readOnly")
            ) =>
            {
                Some(format!(
                    "Codex runs the thread in sandbox {}, wider than workspace-write",
                    result["sandbox"]
                ))
            }
            None => None,
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
            // Under `untrusted`, a command Codex asks about only because its
            // policy says so carries no `reason`. One that carries a reason
            // asks for more: the model's request to run outside the sandbox
            // (`require_escalated`), or Codex's own "retry without sandbox?"
            // after the sandbox refused it. It is its own permission, so an
            // approval of commands never widens Codex's sandbox (verified
            // against codex-cli 0.166.0 on 3 November 2026).
            let escalated = *tool == COMMAND_EXECUTION && !message["params"]["reason"].is_null();
            return Output::event(Event::PermissionRequested {
                turn: self.turns.active,
                request: PermissionRequest {
                    key: PermissionKey(key),
                    tool: if escalated {
                        ESCALATED_COMMAND.to_owned()
                    } else {
                        (*tool).to_owned()
                    },
                    input: self.approval_input(&message["params"]),
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

    /// An approval request's parameters, as the person asked sees them. A
    /// command's carry `command`; a patch's name only the item, so the files
    /// its `item/started` listed are added as `path` (relative to the
    /// thread's working directory), which is what the permission line shows.
    fn approval_input(&self, params: &Value) -> Value {
        let mut input = params.clone();
        let files = params["itemId"]
            .as_str()
            .and_then(|item| self.file_changes.get(item));
        if let (Some(files), Some(map)) = (files, input.as_object_mut()) {
            if !files.is_empty() {
                map.insert("path".into(), json!(files.join(", ")));
            }
        }
        input
    }

    /// The files a `fileChange` item changes, as the thread's working
    /// directory names them.
    fn changed_files(&self, item: &Value) -> Vec<String> {
        let cwd = self.open.as_ref().map(|open| open.cwd.as_str());
        item["changes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|change| change["path"].as_str())
            .map(|path| {
                cwd.and_then(|cwd| path.strip_prefix(cwd))
                    .map_or(path, |rest| rest.trim_start_matches('/'))
                    .to_owned()
            })
            .collect()
    }

    /// What a tool item acted on: its command, its first changed file, or
    /// the MCP server and tool it called. Search and image items name none.
    fn item_target(&self, kind: &str, item: &Value) -> Option<target::ToolTarget> {
        let cwd = self.open.as_ref().map_or("", |open| open.cwd.as_str());
        match kind {
            "commandExecution" => {
                let action = item["commandActions"][0]["command"].as_str();
                let command = |raw| target::ToolTarget::command_in(raw, cwd);
                action
                    .and_then(command)
                    .or_else(|| item["command"].as_str().and_then(command))
            }
            "fileChange" => item["changes"]
                .as_array()?
                .iter()
                .find_map(|change| change["path"].as_str())
                .and_then(|path| target::ToolTarget::file(path, cwd)),
            "mcpToolCall" => {
                target::ToolTarget::mcp(item["server"].as_str(), item["tool"].as_str()?)
            }
            "dynamicToolCall" => target::ToolTarget::mcp(None, item["tool"].as_str()?),
            _ => None,
        }
    }

    fn notification(&mut self, message: &Value, method: &str) -> Output {
        let params = &message["params"];
        if QUIET.contains(&method) {
            return Output::default();
        }
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
                if let ("fileChange", Some(id)) = (kind, item["id"].as_str()) {
                    let files = self.changed_files(item);
                    self.file_changes.insert(id.to_owned(), files);
                }
                if TOOL_ITEMS.contains(&kind) {
                    Output::event(Event::ToolStarted {
                        turn,
                        call_id: item["id"].as_str().unwrap_or_default().to_owned(),
                        name: kind.to_owned(),
                        target: self.item_target(kind, item),
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
                        ..Usage::default()
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
            "item/completed" => {
                if let Some(id) = params["item"]["id"].as_str() {
                    self.file_changes.remove(id);
                }
                Output::default()
            }
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
        self.file_changes.clear();
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

/// Why Codex is given no spending limit.
const BUDGET: &str =
    "Codex app-server has no spending limit: no thread or turn parameter caps cost, and its usage carries tokens, not dollars";

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
            model: true,
            budget: false,
        }
    }

    fn capability_reasons(&self) -> Vec<crate::CapabilityReason> {
        vec![("budget", BUDGET)]
    }

    fn open(&mut self, open: Open) -> Result<Opened, Rejected> {
        if open.cwd.is_empty() {
            return Err(Rejected::InvalidOpen("empty working directory".into()));
        }
        if self.open.is_some() {
            return Err(Rejected::InvalidOpen("the session is already open".into()));
        }
        crate::refuse_budget(&open, BUDGET)?;
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

    fn steer_boundary(&self) -> &'static str {
        // `turn/steer`, recorded once the model response in progress
        // finishes (`docs/harness-integration.md` "Steering a running
        // turn").
        "codex_turn_steer"
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
        self.file_changes.clear();
        self.native_turn = None;
        self.steers.clear();
        self.turns.closed()
    }
}
