//! Amp execute mode with streaming JSON input and output.
//!
//! Launches `amp --execute --stream-json --stream-json-input`, or
//! `amp threads continue <id> --execute --stream-json --stream-json-input`
//! to resume a thread. Frame shapes follow Amp's streaming JSON
//! documentation (<https://ampcode.com/docs/cli/streaming-json>), which
//! describes a Claude Code-compatible stream: a `system`/`init` message
//! naming the thread in `session_id`, `user` and `assistant` messages, and a
//! final `result`. No transcript was recorded: Amp needs its server and an
//! account before it prints any protocol line, so these shapes are
//! documentation-derived and unverified against a binary.
//!
//! With `--stream-json-input`, Amp reads user messages until stdin closes and
//! prints `result` only then, once for the whole process. A turn therefore
//! ends at the first top-level assistant message whose `stop_reason` ends a
//! turn (`end_turn`, `stop_sequence`, `max_tokens`, `refusal`), or at a
//! `result` or `system` error if one comes first. The echoed user message
//! acknowledges the turn. The driver becomes ready on `init`; that Amp
//! prints `init` before it reads the first message is an assumption to
//! verify live.
//!
//! Amp has no fork and no model flag (`--mode` selects an agent mode, not a
//! model), so both are refused before launch. The stream carries no
//! permission requests or cancellation: Amp does not ask before running
//! tools. Branchyard must restrict tools through the private settings file
//! (`amp.tools.disable`, `amp.mcpPermissions`, or a plugin) and must never
//! set `amp.dangerouslyAllowAll`. Never add `-ox` or `--executor`: the
//! thread must execute in Branchyard's sandbox, not on Amp's servers.

use serde_json::json;

use crate::{
    frame, parse, Capabilities, Driver, Event, Frame, LaunchSpec, NativeSession, Open, Opened,
    Output, PermissionDecision, PermissionKey, Rejected, SessionMode, Submitted, TurnOutcome,
    Turns, Usage, Value,
};

/// An Amp execute-mode session with streaming JSON input.
#[derive(Debug)]
pub struct Amp {
    command: Vec<String>,
    mode: Option<SessionMode>,
    ready: bool,
    session: Option<NativeSession>,
    turns: Turns,
    accepted: bool,
}

impl Amp {
    /// `command` is the executable and any fixed leading arguments, usually
    /// `["amp"]`.
    pub fn new(command: Vec<String>) -> Self {
        Self {
            command,
            mode: None,
            ready: false,
            session: None,
            turns: Turns::default(),
            accepted: false,
        }
    }

    fn system(&mut self, message: &Value) -> Output {
        match message["subtype"].as_str().unwrap_or_default() {
            "init" => self.init(message),
            subtype @ ("error_during_execution" | "error_max_turns") => {
                let error = message["error"].as_str().unwrap_or(subtype).to_owned();
                self.error(subtype, error)
            }
            other => Output::event(Event::Unrecognized {
                kind: format!("system/{other}"),
            }),
        }
    }

    fn init(&mut self, message: &Value) -> Output {
        let Some(session) = message["session_id"].as_str().and_then(NativeSession::new) else {
            return Output::event(Event::OpenFailed {
                reason: "system/init without a usable session_id".into(),
            });
        };
        if let Some(known) = &self.session {
            return if *known == session {
                Output::default()
            } else {
                Output::event(Event::ProtocolViolation {
                    detail: format!("thread changed from {known} to {session}"),
                })
            };
        }
        if let Some(SessionMode::Resume(expected)) = &self.mode {
            if *expected != session {
                return Output::event(Event::OpenFailed {
                    reason: format!("resume of {expected} opened thread {session} instead"),
                });
            }
        }
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

    /// An error message or failed `result`: ends the turn in flight, or fails
    /// the open before `init`.
    fn error(&mut self, subtype: &str, error: String) -> Output {
        if !self.ready {
            return Output::event(Event::OpenFailed { reason: error });
        }
        let Some(turn) = self.end_turn() else {
            return Output::event(Event::Warning { message: error });
        };
        let outcome = match subtype {
            "error_max_turns" => TurnOutcome::LimitReached {
                limit: "max_turns".into(),
            },
            _ => TurnOutcome::Failed { message: error },
        };
        Output::event(Event::TurnEnded { turn, outcome })
    }

    fn end_turn(&mut self) -> Option<u64> {
        self.accepted = false;
        self.turns.end()
    }

    fn user(&mut self, message: &Value) -> Output {
        let Some(turn) = self.turns.active else {
            return Output::default();
        };
        let echoed_text = message["parent_tool_use_id"].is_null()
            && message["message"]["content"]
                .as_array()
                .is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "text"));
        if !echoed_text || self.accepted {
            return Output::default();
        }
        self.accepted = true;
        Output::event(Event::TurnAccepted { turn, native: None })
    }

    fn assistant(&mut self, message: &Value) -> Output {
        let Some(turn) = self.turns.active else {
            return Output::event(Event::Unrecognized {
                kind: "assistant message outside a turn".into(),
            });
        };
        // Subagent messages belong to a tool call of this turn.
        if !message["parent_tool_use_id"].is_null() {
            return Output::default();
        }
        let body = &message["message"];
        let mut events = Vec::new();
        for block in body["content"].as_array().into_iter().flatten() {
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
        let usage = &body["usage"];
        if usage.is_object() {
            // Per model response, not a running total.
            events.push(Event::UsageObserved {
                turn: Some(turn),
                usage: Usage {
                    cumulative: false,
                    input_tokens: usage["input_tokens"].as_u64(),
                    output_tokens: usage["output_tokens"].as_u64(),
                    cached_input_tokens: usage["cache_read_input_tokens"].as_u64(),
                    cost_usd: None,
                },
            });
        }
        let outcome = match body["stop_reason"].as_str() {
            Some("end_turn" | "stop_sequence") => Some(TurnOutcome::Completed),
            Some("max_tokens") => Some(TurnOutcome::LimitReached {
                limit: "max_tokens".into(),
            }),
            Some("refusal") => Some(TurnOutcome::Refused),
            // tool_use, pause_turn or null: the agent continues.
            _ => None,
        };
        if let Some(outcome) = outcome {
            self.end_turn();
            events.push(Event::TurnEnded { turn, outcome });
        }
        Output {
            events,
            frames: Vec::new(),
        }
    }

    fn result(&mut self, message: &Value) -> Output {
        if let (Some(known), Some(id)) = (&self.session, message["session_id"].as_str()) {
            if id != known.as_str() {
                return Output::event(Event::ProtocolViolation {
                    detail: format!("result for thread {id}, not {known}"),
                });
            }
        }
        let subtype = message["subtype"].as_str().unwrap_or_default();
        if message["is_error"] == true || subtype != "success" {
            let error = message["error"].as_str().unwrap_or(subtype).to_owned();
            return self.error(subtype, error);
        }
        // The process's final summary after stdin closed.
        match self.end_turn() {
            Some(turn) => Output::event(Event::TurnEnded {
                turn,
                outcome: TurnOutcome::Completed,
            }),
            None => Output::default(),
        }
    }
}

impl Driver for Amp {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            resume: true,
            fork: false,
            cancellation: false,
            tool_approvals: false,
            turn_acknowledgment: true,
            usage: true,
        }
    }

    fn capability_reasons(&self) -> &'static [crate::CapabilityReason] {
        &[
            ("fork", "Amp cannot fork a thread"),
            (
                "cancellation",
                "Amp's streaming input has no cancellation message",
            ),
            (
                "tool_approvals",
                "Amp does not ask before running tools; Branchyard must restrict tools through the private settings file (amp.tools.disable, amp.mcpPermissions, or a plugin) and must never set amp.dangerouslyAllowAll",
            ),
        ]
    }

    fn open(&mut self, open: Open) -> Result<Opened, Rejected> {
        if open.cwd.is_empty() {
            return Err(Rejected::InvalidOpen("empty working directory".into()));
        }
        if self.mode.is_some() {
            return Err(Rejected::InvalidOpen("the session is already open".into()));
        }
        crate::refuse_projection(&open, "Amp")?;
        if let SessionMode::Fork(_) = open.mode {
            return Err(Rejected::Unsupported("Amp cannot fork a thread".into()));
        }
        if open.model.is_some() {
            return Err(Rejected::Unsupported(
                "Amp selects models through agent modes, not a model name".into(),
            ));
        }
        let mut argv = self.command.clone();
        if let SessionMode::Resume(thread) = &open.mode {
            argv.extend(["threads".into(), "continue".into(), thread.to_string()]);
        }
        argv.extend(["--execute", "--stream-json", "--stream-json-input"].map(String::from));
        self.mode = Some(open.mode);
        Ok(Opened {
            launch: LaunchSpec {
                argv,
                cwd: open.cwd,
            },
            // Amp speaks first: `init` arrives without a request.
            frames: Vec::new(),
        })
    }

    fn receive(&mut self, line: &[u8]) -> Output {
        let message = match parse(line) {
            Ok(message) => message,
            Err(output) => return output,
        };
        match message["type"].as_str().unwrap_or_default() {
            "system" => self.system(&message),
            "user" => self.user(&message),
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
        self.accepted = false;
        let message = json!({
            "type": "user",
            "message": {"role": "user", "content": [{"type": "text", "text": prompt}]},
        });
        Ok(Submitted {
            turn,
            frames: vec![frame(&message)],
        })
    }

    fn interrupt(&mut self) -> Result<Vec<Frame>, Rejected> {
        self.turns.active.ok_or(Rejected::NoTurn)?;
        Err(Rejected::Unsupported(
            "Amp's streaming input has no cancellation message".into(),
        ))
    }

    fn respond(
        &mut self,
        _key: &PermissionKey,
        _decision: PermissionDecision,
    ) -> Result<Vec<Frame>, Rejected> {
        // Amp never asks for tool approval on this stream.
        Err(Rejected::UnknownPermission)
    }

    fn transport_closed(&mut self) -> Vec<Event> {
        self.ready = false;
        self.accepted = false;
        self.turns.closed()
    }
}
