//! Pi RPC mode over JSON Lines.
//!
//! Launches `pi --mode rpc`. Frame shapes follow Pi's RPC documentation
//! (`docs/rpc.md`, `docs/rpc-commands.md`, `docs/json.md` and
//! `docs/rpc-extension-ui.md` in `@earendil-works/pi-coding-agent`) and its
//! `dist/modes/rpc/rpc-mode.js`, and were checked against pi 0.87.1,
//! recorded without a model call. Commands carry a string `id` that their
//! `response` repeats; session events carry none.
//!
//! RPC mode has no greeting, so the handshake is a `get_state` request whose
//! response names the session. A `prompt` response only means the prompt
//! was accepted: it is [`Event::TurnAccepted`], and the turn ends at
//! `agent_settled`, after which Pi does no more automatic work (retries,
//! compaction recovery, queued messages). `abort` is answered only after the
//! session is idle, so its acknowledgment follows the turn's end.
//!
//! Resume passes `--session <id>` and fork `--fork <id>`; the `get_state`
//! session ID is checked, since `--session` also accepts an ID prefix. Only
//! bare session IDs are passed: Pi reads any argument containing a path
//! separator or ending in `.jsonl` as a session file path, and a harness
//! reference must never select a host file, so such references are refused
//! before launch. Pi never asks for tool approval (its tools run with the
//! process's own permissions), so this profile offers none: restrict tools
//! with `--tools`/`--no-tools` and confine the process with the sandbox.
//! Extension dialogs are answered as cancelled; prompts beginning with `/`
//! are refused because Pi may run them as extension commands that start no
//! agent run and so never settle.

use std::collections::HashMap;

use serde_json::json;

use crate::{
    frame, parse, Capabilities, Driver, Event, Frame, LaunchSpec, NativeSession, Open, Opened,
    Output, PermissionDecision, PermissionKey, Rejected, SessionMode, Submitted, TurnOutcome,
    Turns, Usage, Value,
};

#[derive(Debug)]
enum Pending {
    State,
    Prompt(u64),
    Abort(u64),
}

/// Extension UI methods that block until the client answers.
const DIALOGS: [&str; 4] = ["select", "confirm", "input", "editor"];

/// A Pi RPC session.
#[derive(Debug)]
pub struct Pi {
    command: Vec<String>,
    mode: Option<SessionMode>,
    ready: bool,
    session: Option<NativeSession>,
    next_id: u64,
    pending: HashMap<String, Pending>,
    turns: Turns,
    interrupting: bool,
    /// The last assistant message's stop reason and error in this turn.
    last_stop: Option<(String, Option<String>)>,
}

impl Pi {
    /// `command` is the executable and any fixed leading arguments, usually
    /// `["pi"]`.
    pub fn new(command: Vec<String>) -> Self {
        Self {
            command,
            mode: None,
            ready: false,
            session: None,
            next_id: 0,
            pending: HashMap::new(),
            turns: Turns::default(),
            interrupting: false,
            last_stop: None,
        }
    }

    fn command(&mut self, pending: Pending, mut command: Value) -> Frame {
        self.next_id += 1;
        let id = format!("branchyard-{}", self.next_id);
        command["id"] = json!(id);
        self.pending.insert(id, pending);
        frame(&command)
    }

    fn response(&mut self, message: &Value) -> Output {
        let Some(pending) = message["id"]
            .as_str()
            .and_then(|id| self.pending.remove(id))
        else {
            return Output::event(Event::ProtocolViolation {
                detail: format!(
                    "response to unknown command {} ({}): {}",
                    message["id"],
                    message["command"].as_str().unwrap_or_default(),
                    message["error"].as_str().unwrap_or_default()
                ),
            });
        };
        let error = (message["success"] != true).then(|| {
            message["error"]
                .as_str()
                .unwrap_or("the command failed")
                .to_owned()
        });
        match (pending, error) {
            (Pending::State, None) => self.state(&message["data"]),
            (Pending::State, Some(reason)) => Output::event(Event::OpenFailed { reason }),
            (Pending::Prompt(turn), None) => Output::event(Event::TurnAccepted {
                turn,
                native: message["id"].as_str().map(str::to_owned),
            }),
            (Pending::Prompt(turn), Some(message)) => {
                self.end_turn();
                Output::event(Event::TurnEnded {
                    turn,
                    outcome: TurnOutcome::Failed { message },
                })
            }
            (Pending::Abort(turn), None) => Output::event(Event::InterruptAcknowledged { turn }),
            (Pending::Abort(_), Some(error)) => Output::event(Event::Warning {
                message: format!("abort failed: {error}"),
            }),
        }
    }

    fn state(&mut self, data: &Value) -> Output {
        let Some(session) = data["sessionId"].as_str().and_then(NativeSession::new) else {
            return Output::event(Event::OpenFailed {
                reason: "get_state without a usable sessionId".into(),
            });
        };
        let forked_from = match &self.mode {
            Some(SessionMode::Resume(expected)) if *expected != session => {
                return Output::event(Event::OpenFailed {
                    reason: format!("resume of {expected} opened session {session} instead"),
                })
            }
            Some(SessionMode::Fork(parent)) if *parent == session => {
                return Output::event(Event::OpenFailed {
                    reason: format!("fork of {parent} kept the parent's session ID"),
                })
            }
            Some(SessionMode::Fork(parent)) => Some(parent.clone()),
            _ => None,
        };
        self.session = Some(session.clone());
        self.ready = true;
        Output {
            events: vec![
                Event::Ready,
                Event::SessionStarted {
                    session,
                    forked_from,
                },
            ],
            frames: Vec::new(),
        }
    }

    fn end_turn(&mut self) -> Option<u64> {
        self.interrupting = false;
        self.last_stop = None;
        self.turns.end()
    }

    fn settled(&mut self) -> Output {
        let interrupted = self.interrupting;
        let last = self.last_stop.take();
        let Some(turn) = self.end_turn() else {
            return Output::default();
        };
        let outcome = match last {
            _ if interrupted => TurnOutcome::Interrupted,
            None => TurnOutcome::Completed,
            Some((reason, error)) => match reason.as_str() {
                "stop" => TurnOutcome::Completed,
                "aborted" => TurnOutcome::Interrupted,
                "length" => TurnOutcome::LimitReached {
                    limit: "max_tokens".into(),
                },
                "error" => TurnOutcome::Failed {
                    message: error.unwrap_or_else(|| "the model call failed".into()),
                },
                other => TurnOutcome::Failed {
                    message: format!("the run settled after stop reason {other:?}"),
                },
            },
        };
        Output::event(Event::TurnEnded { turn, outcome })
    }

    fn message_end(&mut self, turn: u64, message: &Value) -> Output {
        if message["role"] != "assistant" {
            return Output::default();
        }
        self.last_stop = message["stopReason"].as_str().map(|reason| {
            (
                reason.to_owned(),
                message["errorMessage"].as_str().map(str::to_owned),
            )
        });
        let usage = &message["usage"];
        if !usage.is_object() {
            return Output::default();
        }
        // Per assistant response, not a running total.
        Output::event(Event::UsageObserved {
            turn: Some(turn),
            usage: Usage {
                cumulative: false,
                input_tokens: usage["input"].as_u64(),
                output_tokens: usage["output"].as_u64(),
                cached_input_tokens: usage["cacheRead"].as_u64(),
                cost_usd: usage["cost"]["total"].as_f64(),
            },
        })
    }

    fn extension_ui(&mut self, message: &Value) -> Output {
        let method = message["method"].as_str().unwrap_or_default();
        if DIALOGS.contains(&method) {
            let reply =
                json!({"type": "extension_ui_response", "id": message["id"], "cancelled": true});
            return Output {
                events: vec![Event::UnsupportedRequest {
                    method: format!("extension_ui_request/{method}"),
                }],
                frames: vec![frame(&reply)],
            };
        }
        match (method, message["notifyType"].as_str()) {
            ("notify", Some("warning" | "error")) => Output::event(Event::Warning {
                message: message["message"].as_str().unwrap_or_default().to_owned(),
            }),
            // Fire-and-forget display updates need no answer.
            _ => Output::default(),
        }
    }

    fn event(&mut self, kind: &str, message: &Value) -> Output {
        match kind {
            "extension_error" => {
                return Output::event(Event::Warning {
                    message: format!(
                        "extension error in {}: {}",
                        message["event"].as_str().unwrap_or_default(),
                        message["error"].as_str().unwrap_or_default()
                    ),
                })
            }
            "agent_settled" => return self.settled(),
            _ => {}
        }
        let Some(turn) = self.turns.active else {
            return Output::event(Event::Unrecognized {
                kind: kind.to_owned(),
            });
        };
        match kind {
            "message_update" => {
                let update = &message["assistantMessageEvent"];
                match (update["type"].as_str(), update["delta"].as_str()) {
                    (Some("text_delta"), Some(text)) => Output::event(Event::MessageDelta {
                        turn,
                        text: text.to_owned(),
                    }),
                    _ => Output::default(),
                }
            }
            "tool_execution_start" => Output::event(Event::ToolStarted {
                turn,
                call_id: message["toolCallId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                name: message["toolName"].as_str().unwrap_or_default().to_owned(),
            }),
            "message_end" => self.message_end(turn, &message["message"]),
            "auto_retry_start" => Output::event(Event::Warning {
                message: format!(
                    "retry {} of {}: {}",
                    message["attempt"].as_u64().unwrap_or(0),
                    message["maxAttempts"].as_u64().unwrap_or(0),
                    message["errorMessage"].as_str().unwrap_or_default()
                ),
            }),
            "compaction_end" => match message["errorMessage"].as_str() {
                Some(error) => Output::event(Event::Warning {
                    message: format!("compaction failed: {error}"),
                }),
                None => Output::default(),
            },
            "agent_start"
            | "agent_end"
            | "turn_start"
            | "turn_end"
            | "message_start"
            | "tool_execution_update"
            | "tool_execution_end"
            | "auto_retry_end"
            | "compaction_start"
            | "queue_update"
            | "entry_appended" => Output::default(),
            other => Output::event(Event::Unrecognized {
                kind: other.to_owned(),
            }),
        }
    }
}

/// Whether Pi would read `reference` as a session file path rather than an ID.
fn is_path(reference: &NativeSession) -> bool {
    let value = reference.as_str();
    value.contains('/') || value.contains('\\') || value.ends_with(".jsonl")
}

impl Driver for Pi {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            resume: true,
            fork: true,
            cancellation: true,
            tool_approvals: false,
            turn_acknowledgment: true,
            usage: true,
        }
    }

    fn capability_reasons(&self) -> &'static [crate::CapabilityReason] {
        &[(
            "tool_approvals",
            "Pi never asks for tool approval; its tools run with the process's own permissions, so restrict them with --tools/--no-tools and the sandbox instead",
        )]
    }

    fn open(&mut self, open: Open) -> Result<Opened, Rejected> {
        if open.cwd.is_empty() {
            return Err(Rejected::InvalidOpen("empty working directory".into()));
        }
        if self.mode.is_some() {
            return Err(Rejected::InvalidOpen("the session is already open".into()));
        }
        crate::refuse_projection(&open, "Pi")?;
        let mut argv = self.command.clone();
        argv.extend(["--mode".into(), "rpc".into()]);
        if let Some(model) = &open.model {
            argv.extend(["--model".into(), model.clone()]);
        }
        match &open.mode {
            SessionMode::Fresh => {}
            SessionMode::Resume(reference) | SessionMode::Fork(reference) if is_path(reference) => {
                return Err(Rejected::InvalidOpen(format!(
                    "{reference} is a session file path; only session IDs are accepted"
                )))
            }
            SessionMode::Resume(session) => argv.extend(["--session".into(), session.to_string()]),
            SessionMode::Fork(session) => argv.extend(["--fork".into(), session.to_string()]),
        }
        self.mode = Some(open.mode);
        let state = self.command(Pending::State, json!({"type": "get_state"}));
        Ok(Opened {
            launch: LaunchSpec {
                argv,
                cwd: open.cwd,
            },
            frames: vec![state],
        })
    }

    fn receive(&mut self, line: &[u8]) -> Output {
        let message = match parse(line) {
            Ok(message) => message,
            Err(output) => return output,
        };
        match message["type"].as_str().unwrap_or_default() {
            "response" => self.response(&message),
            "extension_ui_request" => self.extension_ui(&message),
            "" => Output::event(Event::ProtocolViolation {
                detail: "record without a type".into(),
            }),
            kind => self.event(kind, &message),
        }
    }

    fn submit(&mut self, prompt: &str) -> Result<Submitted, Rejected> {
        if !self.ready {
            return Err(Rejected::NotReady);
        }
        if prompt.starts_with('/') {
            return Err(Rejected::Unsupported(
                "a prompt beginning with / can run a Pi command that never settles".into(),
            ));
        }
        let turn = self.turns.begin()?;
        self.last_stop = None;
        let command = self.command(
            Pending::Prompt(turn),
            json!({"type": "prompt", "message": prompt}),
        );
        Ok(Submitted {
            turn,
            frames: vec![command],
        })
    }

    fn interrupt(&mut self) -> Result<Vec<Frame>, Rejected> {
        let turn = self.turns.active.ok_or(Rejected::NoTurn)?;
        self.interrupting = true;
        Ok(vec![
            self.command(Pending::Abort(turn), json!({"type": "abort"}))
        ])
    }

    fn respond(
        &mut self,
        _key: &PermissionKey,
        _decision: PermissionDecision,
    ) -> Result<Vec<Frame>, Rejected> {
        // Pi never asks for tool approval.
        Err(Rejected::UnknownPermission)
    }

    fn transport_closed(&mut self) -> Vec<Event> {
        self.ready = false;
        self.pending.clear();
        self.interrupting = false;
        self.last_stop = None;
        self.turns.closed()
    }
}
