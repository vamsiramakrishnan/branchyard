//! Antigravity CLI headless streaming over NDJSON.
//!
//! Launches `agy --input-format stream-json --output-format stream-json
//! --disable-slash-commands`. Frame shapes follow the headless-mode
//! documentation (<https://antigravity.google/docs/cli/headless/>) and were
//! checked against Antigravity CLI 1.2.11, recorded without a model call:
//! every line is an object whose `event` field names it; the CLI prints one
//! `init` at startup, before any prompt, carrying the `conversation_id`; each
//! `{"event":"user","message":{"content":...}}` line runs one turn of
//! `step_update` events ending in exactly one `result`.
//!
//! Resume passes `--conversation <id>`. Antigravity 1.2.11 starts a *new*
//! conversation, warning only on stderr, when the ID is unknown, so the
//! driver checks the `init` conversation ID and fails the open on a
//! mismatch. The CLI has no fork. `--disable-slash-commands` keeps a prompt
//! beginning with `/` literal; without it, commands the CLI answers itself,
//! such as `/model`, end the session.
//!
//! The stream carries no permission requests and no control messages:
//! control frames end the session with an error, and a tool that needs
//! approval is soft-denied under the permission rules in the private
//! `~/.gemini/antigravity-cli/settings.json`. Branchyard must write those
//! rules (`permissions.allow`) into the harness's home and must never pass
//! `--dangerously-skip-permissions`. There is no in-band cancellation;
//! SIGINT ends the whole process.
//!
//! `result.usage` covers the whole session, not the turn, so usage is
//! reported as cumulative.

use std::collections::BTreeSet;

use serde_json::json;

use crate::{
    frame, parse, Capabilities, Driver, Event, Frame, LaunchSpec, NativeSession, Open, Opened,
    Output, PermissionDecision, PermissionKey, Rejected, SessionMode, Submitted, TurnOutcome,
    Turns, Usage, Value,
};

/// An Antigravity CLI streaming session.
#[derive(Debug)]
pub struct Antigravity {
    command: Vec<String>,
    mode: Option<SessionMode>,
    ready: bool,
    session: Option<NativeSession>,
    turns: Turns,
    /// Whether the turn in flight has been acknowledged by a `user_input` step.
    accepted: bool,
    /// Tool steps already reported in the turn in flight, by step index.
    tools: BTreeSet<u64>,
}

impl Antigravity {
    /// `command` is the executable and any fixed leading arguments, usually
    /// `["agy"]`.
    pub fn new(command: Vec<String>) -> Self {
        Self {
            command,
            mode: None,
            ready: false,
            session: None,
            turns: Turns::default(),
            accepted: false,
            tools: BTreeSet::new(),
        }
    }

    fn init(&mut self, message: &Value) -> Output {
        let Some(session) = message["conversation_id"]
            .as_str()
            .and_then(NativeSession::new)
        else {
            return Output::event(Event::OpenFailed {
                reason: "init without a usable conversation_id".into(),
            });
        };
        if let Some(known) = &self.session {
            return Output::event(Event::ProtocolViolation {
                detail: format!("a second init, for {session}, in conversation {known}"),
            });
        }
        if let Some(SessionMode::Resume(expected)) = &self.mode {
            if *expected != session {
                return Output::event(Event::OpenFailed {
                    reason: format!("resume of {expected} opened conversation {session} instead"),
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

    fn step(&mut self, step: &Value) -> Output {
        let Some(turn) = self.turns.active else {
            return Output::event(Event::Unrecognized {
                kind: "step_update outside a turn".into(),
            });
        };
        let index = step["step_index"].as_u64();
        let mut events = Vec::new();
        match step["step_type"].as_str().unwrap_or_default() {
            "user_input" if !self.accepted => {
                self.accepted = true;
                events.push(Event::TurnAccepted {
                    turn,
                    native: index.map(|i| format!("step {i}")),
                });
            }
            "agent_response" => {
                if let Some(text) = step["text_delta"].as_str().filter(|t| !t.is_empty()) {
                    events.push(Event::MessageDelta {
                        turn,
                        text: text.to_owned(),
                    });
                }
            }
            "tool" => {
                if index.is_none_or(|i| self.tools.insert(i)) {
                    let name = step["tool_name"]
                        .as_str()
                        .or_else(|| step["tool_info"]["name"].as_str())
                        .unwrap_or_default();
                    events.push(Event::ToolStarted {
                        turn,
                        call_id: index.map(|i| i.to_string()).unwrap_or_default(),
                        name: name.to_owned(),
                    });
                }
            }
            "error_message" => events.push(Event::Warning {
                message: match index {
                    Some(i) => format!("step {i} reported an error"),
                    None => "a step reported an error".into(),
                },
            }),
            _ => {}
        }
        Output {
            events,
            frames: Vec::new(),
        }
    }

    fn result(&mut self, result: &Value) -> Output {
        let error = result["error"].as_str().unwrap_or_default();
        if !self.ready {
            return Output::event(Event::OpenFailed {
                reason: if error.is_empty() {
                    "the CLI ended before init".into()
                } else {
                    error.to_owned()
                },
            });
        }
        if let (Some(known), Some(id)) = (&self.session, result["conversation_id"].as_str()) {
            if !id.is_empty() && id != known.as_str() {
                return Output::event(Event::ProtocolViolation {
                    detail: format!("result for conversation {id}, not {known}"),
                });
            }
        }
        let Some(turn) = self.turns.end() else {
            return Output::event(Event::ProtocolViolation {
                detail: format!("result without a turn in flight: {error}"),
            });
        };
        self.accepted = false;
        self.tools.clear();
        let status = result["status"].as_str().unwrap_or_default();
        let outcome = match status {
            "SUCCESS" => TurnOutcome::Completed,
            "CANCELED" | "INTERRUPTED" => TurnOutcome::Interrupted,
            _ => TurnOutcome::Failed {
                message: if error.is_empty() {
                    format!("the turn ended with status {status:?}")
                } else {
                    error.to_owned()
                },
            },
        };
        let mut events = Vec::new();
        if result["usage"].is_object() {
            let usage = &result["usage"];
            events.push(Event::UsageObserved {
                turn: Some(turn),
                usage: Usage {
                    cumulative: true,
                    input_tokens: usage["input_tokens"].as_u64(),
                    output_tokens: usage["output_tokens"].as_u64(),
                    cached_input_tokens: usage["cache_read_tokens"].as_u64(),
                    cost_usd: None,
                },
            });
        }
        events.push(Event::TurnEnded { turn, outcome });
        Output {
            events,
            frames: Vec::new(),
        }
    }
}

impl Driver for Antigravity {
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

    fn open(&mut self, open: Open) -> Result<Opened, Rejected> {
        if open.cwd.is_empty() {
            return Err(Rejected::InvalidOpen("empty working directory".into()));
        }
        if self.mode.is_some() {
            return Err(Rejected::InvalidOpen("the session is already open".into()));
        }
        crate::refuse_projection(&open, "the Antigravity CLI")?;
        if let SessionMode::Fork(_) = open.mode {
            return Err(Rejected::Unsupported(
                "the Antigravity CLI cannot fork a conversation".into(),
            ));
        }
        let mut argv = self.command.clone();
        argv.extend(
            [
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--disable-slash-commands",
            ]
            .map(String::from),
        );
        if let Some(model) = &open.model {
            argv.extend(["--model".into(), model.clone()]);
        }
        if let SessionMode::Resume(conversation) = &open.mode {
            argv.extend(["--conversation".into(), conversation.to_string()]);
        }
        self.mode = Some(open.mode);
        Ok(Opened {
            launch: LaunchSpec {
                argv,
                cwd: open.cwd,
            },
            // The CLI speaks first: `init` arrives without a request.
            frames: Vec::new(),
        })
    }

    fn receive(&mut self, line: &[u8]) -> Output {
        let message = match parse(line) {
            Ok(message) => message,
            Err(output) => return output,
        };
        match message["event"].as_str().unwrap_or_default() {
            "init" => self.init(&message),
            "step_update" => self.step(&message["step_update"]),
            "result" => self.result(&message["result"]),
            "" => Output::event(Event::ProtocolViolation {
                detail: "message without an event field".into(),
            }),
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
        self.tools.clear();
        let message = json!({"event": "user", "message": {"content": prompt}});
        Ok(Submitted {
            turn,
            frames: vec![frame(&message)],
        })
    }

    fn interrupt(&mut self) -> Result<Vec<Frame>, Rejected> {
        self.turns.active.ok_or(Rejected::NoTurn)?;
        Err(Rejected::Unsupported(
            "the Antigravity stream has no cancellation message; a signal ends the whole process"
                .into(),
        ))
    }

    fn respond(
        &mut self,
        _key: &PermissionKey,
        _decision: PermissionDecision,
    ) -> Result<Vec<Frame>, Rejected> {
        // The stream never carries a permission request.
        Err(Rejected::UnknownPermission)
    }

    fn transport_closed(&mut self) -> Vec<Event> {
        self.ready = false;
        self.accepted = false;
        self.tools.clear();
        self.turns.closed()
    }
}
