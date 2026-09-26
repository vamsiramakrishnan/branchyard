//! Claude Code print mode over stream-json.
//!
//! Launches `claude -p` with stream-json input and output and
//! `--permission-prompt-tool stdio`, the same launch the Agent SDK uses. Frame
//! shapes follow the stdout protocol types the Agent SDK publishes
//! (`StdoutMessage` in `@anthropic-ai/claude-agent-sdk` 0.3.283, paired with
//! Claude Code 2.1.283): user messages in, `system`/`assistant`/`result`
//! messages out, and `control_request`/`control_response` frames for the
//! handshake, interrupts and `can_use_tool` permission prompts.
//!
//! Resume passes `--resume <id>`; fork adds `--fork-session`. The session ID
//! arrives on `system/init` and is checked: a resume that comes back under a
//! different ID, or a fork that keeps the parent's, is a protocol violation,
//! never silently accepted.

use std::collections::HashMap;

use serde_json::json;

use crate::{
    frame, parse, Capabilities, Driver, Event, Frame, LaunchSpec, NativeSession, Open, Opened,
    Output, PermissionDecision, PermissionKey, PermissionRequest, Rejected, SessionMode, Submitted,
    TurnOutcome, Turns, Usage, Value,
};

#[derive(Debug)]
enum Pending {
    Initialize,
    Interrupt(u64),
}

/// A Claude Code stream-json session.
#[derive(Debug)]
pub struct ClaudeCode {
    command: Vec<String>,
    mode: Option<SessionMode>,
    ready: bool,
    session: Option<NativeSession>,
    next_request: u64,
    pending: HashMap<String, Pending>,
    permissions: HashMap<String, Value>,
    turns: Turns,
    /// The client UUID stamped on the turn in flight, and whether the CLI has
    /// acknowledged it.
    turn_uuid: Option<(String, bool)>,
    interrupting: bool,
}

impl ClaudeCode {
    /// `command` is the executable and any fixed leading arguments, usually
    /// `["claude"]`.
    pub fn new(command: Vec<String>) -> Self {
        Self {
            command,
            mode: None,
            ready: false,
            session: None,
            next_request: 0,
            pending: HashMap::new(),
            permissions: HashMap::new(),
            turns: Turns::default(),
            turn_uuid: None,
            interrupting: false,
        }
    }

    fn control_request(&mut self, pending: Pending, request: Value) -> Frame {
        self.next_request += 1;
        let id = format!("branchyard-{}", self.next_request);
        let request = json!({"type": "control_request", "request_id": id, "request": request});
        self.pending.insert(id, pending);
        frame(&request)
    }

    fn control_response(&mut self, message: &Value) -> Output {
        let response = &message["response"];
        let id = response["request_id"].as_str().unwrap_or_default();
        let success = response["subtype"] == "success";
        let error = response["error"]
            .as_str()
            .unwrap_or("unspecified error")
            .to_owned();
        match self.pending.remove(id) {
            Some(Pending::Initialize) if success => {
                self.ready = true;
                Output::event(Event::Ready)
            }
            Some(Pending::Initialize) => Output::event(Event::OpenFailed {
                reason: format!("initialize failed: {error}"),
            }),
            Some(Pending::Interrupt(turn)) if success => {
                Output::event(Event::InterruptAcknowledged { turn })
            }
            Some(Pending::Interrupt(_)) => Output::event(Event::Warning {
                message: format!("interrupt failed: {error}"),
            }),
            None => Output::event(Event::ProtocolViolation {
                detail: format!("control response for unknown request {id:?}"),
            }),
        }
    }

    fn control_request_received(&mut self, message: &Value) -> Output {
        let id = message["request_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let request = &message["request"];
        let subtype = request["subtype"].as_str().unwrap_or_default();
        if subtype == "can_use_tool" && !id.is_empty() {
            let input = request["input"].clone();
            self.permissions.insert(id.clone(), input.clone());
            return Output::event(Event::PermissionRequested {
                turn: self.turns.active,
                request: PermissionRequest {
                    key: PermissionKey(id),
                    tool: request["tool_name"].as_str().unwrap_or_default().to_owned(),
                    input,
                },
            });
        }
        // Hook callbacks, SDK MCP servers and dialogs are not part of this
        // profile. Answer so the CLI does not wait forever.
        let reply = json!({
            "type": "control_response",
            "response": {
                "subtype": "error",
                "request_id": id,
                "error": format!("branchyard does not handle {subtype:?} requests"),
            },
        });
        Output {
            events: vec![Event::UnsupportedRequest {
                method: format!("control_request/{subtype}"),
            }],
            frames: vec![frame(&reply)],
        }
    }

    fn system(&mut self, message: &Value) -> Output {
        let subtype = message["subtype"].as_str().unwrap_or_default();
        match subtype {
            "init" => self.init(message),
            "api_retry" => Output::event(Event::Warning {
                message: format!(
                    "API retry {} of {}",
                    message["attempt"].as_u64().unwrap_or(0),
                    message["max_retries"].as_u64().unwrap_or(0)
                ),
            }),
            other => Output::event(Event::Unrecognized {
                kind: format!("system/{other}"),
            }),
        }
    }

    fn init(&mut self, message: &Value) -> Output {
        let Some(session) = message["session_id"].as_str().and_then(NativeSession::new) else {
            return Output::event(Event::ProtocolViolation {
                detail: "system/init without a usable session_id".into(),
            });
        };
        if let Some(known) = &self.session {
            // The CLI repeats init on later turns of the same session.
            return if *known == session {
                Output::default()
            } else {
                Output::event(Event::ProtocolViolation {
                    detail: format!("session changed from {known} to {session}"),
                })
            };
        }
        let forked_from = match &self.mode {
            Some(SessionMode::Resume(expected)) if *expected != session => {
                return Output::event(Event::ProtocolViolation {
                    detail: format!("resume of {expected} started session {session} instead"),
                })
            }
            Some(SessionMode::Fork(parent)) if *parent == session => {
                return Output::event(Event::ProtocolViolation {
                    detail: format!("fork of {parent} kept the parent's session ID"),
                })
            }
            Some(SessionMode::Fork(parent)) => Some(parent.clone()),
            _ => None,
        };
        self.session = Some(session.clone());
        Output::event(Event::SessionStarted {
            session,
            forked_from,
        })
    }

    /// Record the CLI's first acknowledgment of the turn in flight.
    fn acknowledge(&mut self, uuid: Option<&str>) -> Option<Event> {
        let (expected, acknowledged) = self.turn_uuid.as_mut()?;
        if *acknowledged || uuid != Some(expected.as_str()) {
            return None;
        }
        *acknowledged = true;
        Some(Event::TurnAccepted {
            turn: self.turns.active?,
            native: Some(expected.clone()),
        })
    }

    fn assistant(&mut self, message: &Value) -> Output {
        let mut events: Vec<_> = self
            .acknowledge(message["user_message_uuid"].as_str())
            .into_iter()
            .collect();
        let Some(turn) = self.turns.active else {
            return Output {
                events,
                frames: Vec::new(),
            };
        };
        for block in message["message"]["content"]
            .as_array()
            .into_iter()
            .flatten()
        {
            match block["type"].as_str() {
                Some("text") => events.push(Event::MessageDelta {
                    turn,
                    text: block["text"].as_str().unwrap_or_default().to_owned(),
                }),
                Some("tool_use") => events.push(Event::ToolStarted {
                    turn,
                    call_id: block["id"].as_str().unwrap_or_default().to_owned(),
                    name: block["name"].as_str().unwrap_or_default().to_owned(),
                }),
                _ => {}
            }
        }
        Output {
            events,
            frames: Vec::new(),
        }
    }

    fn result(&mut self, message: &Value) -> Output {
        let mut events = Vec::new();
        if let (Some((expected, _)), Some(echoed)) =
            (&self.turn_uuid, message["user_message_uuid"].as_str())
        {
            if expected != echoed {
                return Output::event(Event::ProtocolViolation {
                    detail: format!("result answers {echoed}, not the turn in flight {expected}"),
                });
            }
        }
        let Some(turn) = self.turns.end() else {
            return Output::event(Event::ProtocolViolation {
                detail: "result without a turn in flight".into(),
            });
        };
        self.turn_uuid = None;
        let interrupted = std::mem::take(&mut self.interrupting);
        events.push(Event::UsageObserved {
            turn: Some(turn),
            usage: usage(message),
        });
        events.push(Event::TurnEnded {
            turn,
            outcome: outcome(message, interrupted),
        });
        Output {
            events,
            frames: Vec::new(),
        }
    }
}

/// Session totals from `modelUsage`, which the SDK documents as the correct
/// accounting field: cumulative and covering subagents.
fn usage(message: &Value) -> Usage {
    let models: Vec<&Value> = message["modelUsage"]
        .as_object()
        .map(|m| m.values().collect())
        .unwrap_or_default();
    let sum = |field: &str| -> Option<u64> {
        if models.is_empty() {
            return None;
        }
        models.iter().map(|m| m[field].as_u64()).sum()
    };
    Usage {
        cumulative: true,
        input_tokens: sum("inputTokens"),
        output_tokens: sum("outputTokens"),
        cached_input_tokens: sum("cacheReadInputTokens"),
        cost_usd: message["total_cost_usd"].as_f64(),
    }
}

fn outcome(message: &Value, interrupted: bool) -> TurnOutcome {
    let reason = message["terminal_reason"].as_str().unwrap_or_default();
    if interrupted || reason.starts_with("aborted") {
        return TurnOutcome::Interrupted;
    }
    match message["subtype"].as_str().unwrap_or_default() {
        "success" if message["is_error"] != true => match message["stop_reason"].as_str() {
            Some("refusal") => TurnOutcome::Refused,
            _ => TurnOutcome::Completed,
        },
        "success" => TurnOutcome::Failed {
            message: message["result"].as_str().unwrap_or("error").to_owned(),
        },
        limit @ ("error_max_turns"
        | "error_max_budget_usd"
        | "error_max_structured_output_retries") => TurnOutcome::LimitReached {
            limit: limit.trim_start_matches("error_").to_owned(),
        },
        _ => TurnOutcome::Failed {
            message: message["errors"]
                .as_array()
                .map(|errors| {
                    errors
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("; ")
                })
                .filter(|joined| !joined.is_empty())
                .unwrap_or_else(|| reason.to_owned()),
        },
    }
}

/// A random version 4 UUID from the standard library's seeded hasher.
fn uuid_v4() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut bytes = [0u8; 16];
    for (i, chunk) in bytes.chunks_mut(8).enumerate() {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u128(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        );
        hasher.write_usize(i);
        chunk.copy_from_slice(&hasher.finish().to_le_bytes());
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

impl Driver for ClaudeCode {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            resume: true,
            fork: true,
            cancellation: true,
            tool_approvals: true,
            turn_acknowledgment: true,
            usage: true,
        }
    }

    fn open(&mut self, open: Open) -> Result<Opened, Rejected> {
        if open.cwd.is_empty() {
            return Err(Rejected::InvalidOpen("empty working directory".into()));
        }
        if self.mode.is_some() {
            return Err(Rejected::InvalidOpen("the session is already open".into()));
        }
        let mut argv = self.command.clone();
        argv.extend(
            [
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--permission-prompt-tool",
                "stdio",
            ]
            .map(String::from),
        );
        if let Some(model) = &open.model {
            argv.extend(["--model".into(), model.clone()]);
        }
        match &open.mode {
            SessionMode::Fresh => {}
            SessionMode::Resume(session) => argv.extend(["--resume".into(), session.to_string()]),
            SessionMode::Fork(session) => {
                argv.extend([
                    "--resume".into(),
                    session.to_string(),
                    "--fork-session".into(),
                ]);
            }
        }
        self.mode = Some(open.mode);
        let initialize =
            self.control_request(Pending::Initialize, json!({"subtype": "initialize"}));
        Ok(Opened {
            launch: LaunchSpec {
                argv,
                cwd: open.cwd,
            },
            frames: vec![initialize],
        })
    }

    fn receive(&mut self, line: &[u8]) -> Output {
        let message = match parse(line) {
            Ok(message) => message,
            Err(output) => return output,
        };
        match message["type"].as_str().unwrap_or_default() {
            "control_response" => self.control_response(&message),
            "control_request" => self.control_request_received(&message),
            "control_cancel_request" => {
                let id = message["request_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                if self.permissions.remove(&id).is_some() {
                    Output::event(Event::PermissionWithdrawn {
                        key: PermissionKey(id),
                    })
                } else {
                    Output::default()
                }
            }
            "keep_alive" | "user" | "stream_event" => Output::default(),
            "command_lifecycle" => {
                let started = matches!(message["state"].as_str(), Some("queued" | "started"));
                let uuid = message["command_uuid"].as_str();
                Output {
                    events: started
                        .then(|| self.acknowledge(uuid))
                        .flatten()
                        .into_iter()
                        .collect(),
                    frames: Vec::new(),
                }
            }
            "system" => self.system(&message),
            "assistant" => self.assistant(&message),
            "result" => self.result(&message),
            other => Output::event(Event::Unrecognized {
                kind: other.to_owned(),
            }),
        }
    }

    fn submit(&mut self, prompt: &str) -> Result<Submitted, Rejected> {
        if !self.ready {
            return Err(Rejected::NotReady);
        }
        let turn = self.turns.begin()?;
        let uuid = uuid_v4();
        let message = json!({
            "type": "user",
            "message": {"role": "user", "content": [{"type": "text", "text": prompt}]},
            "parent_tool_use_id": null,
            "session_id": "",
            "uuid": uuid,
        });
        self.turn_uuid = Some((uuid, false));
        Ok(Submitted {
            turn,
            frames: vec![frame(&message)],
        })
    }

    fn interrupt(&mut self) -> Result<Vec<Frame>, Rejected> {
        let turn = self.turns.active.ok_or(Rejected::NoTurn)?;
        self.interrupting = true;
        Ok(vec![self.control_request(
            Pending::Interrupt(turn),
            json!({"subtype": "interrupt"}),
        )])
    }

    fn respond(
        &mut self,
        key: &PermissionKey,
        decision: PermissionDecision,
    ) -> Result<Vec<Frame>, Rejected> {
        let input = self
            .permissions
            .remove(&key.0)
            .ok_or(Rejected::UnknownPermission)?;
        let result = match decision {
            PermissionDecision::Allow => json!({"behavior": "allow", "updatedInput": input}),
            PermissionDecision::Deny { message } => json!({"behavior": "deny", "message": message}),
        };
        Ok(vec![frame(&json!({
            "type": "control_response",
            "response": {"subtype": "success", "request_id": key.0, "response": result},
        }))])
    }

    fn transport_closed(&mut self) -> Vec<Event> {
        self.ready = false;
        self.permissions.clear();
        self.pending.clear();
        self.turn_uuid = None;
        self.turns.closed()
    }
}
