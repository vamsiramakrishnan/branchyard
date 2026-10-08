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
//! MCP servers go in one `--mcp-config` argument as the JSON the Agent SDK
//! builds from its `mcpServers` option (`{"mcpServers": {name: {"type":
//! "stdio", command, args, env}}}`). Claude Code adds them to the servers it
//! already loads; it is not given `--strict-mcp-config`. A command line is
//! readable by every process on the host (`/proc/<pid>/cmdline`), so when
//! the caller gives [`Open::mcp_config_file`], a file already holding
//! [`mcp_config`], the argument is its path; Claude Code 2.1.283 reads a
//! path there as well as JSON. Without one, servers are passed inline only
//! when none has variables or headers, which may hold tokens; otherwise the
//! open is refused. HTTP and SSE servers are `{"type": "http" | "sse", url,
//! headers}` entries, which 2.1.283 connects to with those headers.
//!
//! Instructions arrive as a plugin (`--plugin-dir`), whose skills Claude Code
//! 2.1.283 lists in its `initialize` response as `<plugin>:<skill>`, or
//! else through `--append-system-prompt`. Neither touches the working tree.
//!
//! Resume passes `--resume <id>`; fork adds `--fork-session`. The session ID
//! arrives on `system/init` and is checked: a resume that comes back under a
//! different ID, or a fork that keeps the parent's, is a protocol violation,
//! never silently accepted.
//!
//! Steering writes another user message while the turn runs, as the Agent
//! SDK does with streaming input. Claude Code 2.1.283, checked against a
//! local stand-in API with no model call, queues it (`command_lifecycle`
//! `queued`, reported as [`Event::SteerAccepted`]) and delivers it before
//! its next model call: when the turn continues after a tool result, into
//! that same request, as a system reminder that the user sent a message
//! while it was working, and the turn's one `result` lists both messages
//! in `user_message_uuids`. When the turn would end without another model
//! call, the CLI runs the queued message right after as a follow-up with a
//! `result` of its own; the driver keeps the turn open until a `result`
//! has answered every steered message, so a steered turn still ends once.
//! An interrupt with steered messages still queued is sent with
//! `cancel_queued: true` (advertised as `interrupt_cancel_queued_v1`), so
//! they are cancelled rather than run after the interrupt; their
//! cancellation is [`Event::SteerRejected`].
//!
//! Each user message the driver writes carries a `uuid`, and a `result`
//! lists the messages it answered in `user_message_uuids`. A result that
//! answers none of the turn's is a turn the CLI ran of its own: on
//! `--resume`, Claude Code 2.1.293 first runs a notification queued in the
//! earlier session (a stopped background task's `<task-notification>`) as
//! a turn with its own `result`. It is reported as a [`Event::Warning`]
//! with its cost, its text is not the turn's, and the turn goes on until
//! its own result (`tests/fixtures/claude-code-2.1.293-resume-notification.jsonl`).
//!
//! Closing the session sends the `end_session` control request before the
//! input closes. Claude Code 2.1.293 does not exit when its input closes
//! while a background task of its own runs (a `Bash` command with
//! `run_in_background`, a monitor, a subagent): it waits for the task,
//! runs another model turn on the task's notification, and only then
//! exits. `end_session` makes it stop those tasks (`task_updated` with
//! `killed`, `task_notification` with `stopped`) and exit at once, checked
//! against the real CLI (`tests/fixtures/claude-code-2.1.293-*`).
//!
//! Live cost: each `assistant` frame carries the usage of the model call
//! it came from. The driver reports each call once, as it grows, as a
//! per-call [`Usage`] with the model's name and no cost, so a consumer
//! can price the turn while it runs; the frame's output count is the one
//! streamed so far, so the estimate runs low until the `result`, whose
//! cumulative `total_cost_usd` replaces it.
//!
//! Every message type Claude Code 2.1.293 prints is mapped or ignored on
//! purpose (`receive` and `system` below give each its reason); only a
//! type it did not print is [`Event::Unrecognized`].

use std::collections::HashMap;

use serde_json::json;

use crate::{
    frame, parse, Capabilities, Driver, Event, Frame, HarnessTask, Instructions, LaunchSpec,
    McpServer, NativeSession, Open, Opened, Output, PermissionDecision, PermissionKey,
    PermissionRequest, Rejected, RemoteMcpServer, SessionMode, Submitted, TurnOutcome, Turns,
    Usage, Value,
};

#[derive(Debug)]
enum Pending {
    Initialize,
    Interrupt(u64),
    EndSession,
}

/// The usage of one model call reported so far: its message ID and its
/// token counts as last reported.
#[derive(Debug, Default)]
struct Call {
    id: String,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    cache_write_1h: u64,
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
    /// Steered messages of the turn in flight that no `result` has answered
    /// yet and the CLI has not cancelled.
    steers: Vec<Steer>,
    next_steer: u64,
    /// The model call whose usage was reported last.
    call: Option<Call>,
    /// The last event of the turn in flight was assistant text, so another
    /// text block starts a new paragraph.
    after_text: bool,
}

/// One steered user message.
#[derive(Debug)]
struct Steer {
    number: u64,
    uuid: String,
    accepted: bool,
    /// The CLI started a turn cycle with it.
    started: bool,
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
            steers: Vec::new(),
            next_steer: 0,
            call: None,
            after_text: false,
        }
    }

    /// `command_lifecycle` for a steered message: queued is its
    /// acceptance; cancelled, discarded and refused drop it undelivered.
    fn steer_lifecycle(&mut self, uuid: &str, state: &str) -> Option<Event> {
        let turn = self.turns.active?;
        let at = self.steers.iter().position(|s| s.uuid == uuid)?;
        match state {
            "queued" if !self.steers[at].accepted => {
                self.steers[at].accepted = true;
                Some(Event::SteerAccepted {
                    turn,
                    steer: self.steers[at].number,
                })
            }
            "started" => {
                self.steers[at].started = true;
                None
            }
            "cancelled" | "discarded" | "refused" => {
                let steer = self.steers.remove(at);
                Some(Event::SteerRejected {
                    turn,
                    steer: steer.number,
                    reason: format!("Claude Code reported the message {state}"),
                })
            }
            _ => None,
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
            Some(Pending::EndSession) if success => Output::default(),
            Some(Pending::EndSession) => Output::event(Event::Warning {
                message: format!("Claude Code refused to end its session: {error}"),
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
        let text = |field: &str| message[field].as_str().unwrap_or_default().to_owned();
        match subtype {
            "init" => self.init(message),
            "api_retry" => Output::event(Event::Warning {
                message: format!(
                    "API retry {} of {}",
                    message["attempt"].as_u64().unwrap_or(0),
                    message["max_retries"].as_u64().unwrap_or(0)
                ),
            }),
            // A shell command, subagent or monitor of the CLI's own; also
            // reported for a foreground command, with `is_backgrounded`
            // false.
            "task_started" => Output::event(Event::HarnessTaskStarted {
                task: task(message),
                background: message["is_backgrounded"] == true,
            }),
            "task_notification" => Output::event(Event::HarnessTaskEnded {
                task_id: text("task_id"),
                status: text("status"),
                summary: message["summary"].as_str().map(str::to_owned),
            }),
            "background_tasks_changed" => Output::event(Event::BackgroundTasks {
                running: message["tasks"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(task)
                    .collect(),
            }),
            // The model is producing (thinking) tokens, or a hook or the
            // CLI itself is busy: alive, with nothing else to say.
            "thinking_tokens" | "status" | "hook_started" | "hook_progress" | "hook_response" => {
                Output::event(Event::Progress {
                    turn: self.turns.active,
                })
            }
            // The model changed under the turn: worth a line in the log.
            "model_fallback" | "model_refusal_fallback" | "model_consent_fallback" => {
                Output::event(Event::Warning {
                    message: format!(
                        "Claude Code switched models ({subtype}){}",
                        message["model"]
                            .as_str()
                            .map(|m| format!(" to {m}"))
                            .unwrap_or_default()
                    ),
                })
            }
            "api_error" => Output::event(Event::Warning {
                message: format!(
                    "API error{}",
                    message["error"]
                        .as_str()
                        .map(|e| format!(": {e}"))
                        .unwrap_or_default()
                ),
            }),
            // Ignored on purpose, each for its reason:
            // - task_updated and task_progress: steps of a task whose start
            //   and end are reported above;
            // - task_summary and post_turn_summary: the CLI's own one-line
            //   summaries for its UI; the turn's text and result say more;
            // - vcs_state_changed: the CLI saw the repository change, which
            //   Branchyard reads for itself;
            // - compact_boundary and microcompact_boundary: the CLI trimmed
            //   its context, which changes nothing Branchyard tracks;
            // - session_state_changed, session_metadata,
            //   session_title_changed, commands_changed, notification,
            //   informational, local_command, turn_duration,
            //   stop_hook_summary, files_persisted, permission_denied and
            //   away_summary: UI state of an interactive session.
            "task_updated"
            | "task_progress"
            | "task_summary"
            | "post_turn_summary"
            | "vcs_state_changed"
            | "compact_boundary"
            | "microcompact_boundary"
            | "session_state_changed"
            | "session_metadata"
            | "session_title_changed"
            | "commands_changed"
            | "notification"
            | "informational"
            | "local_command"
            | "turn_duration"
            | "stop_hook_summary"
            | "files_persisted"
            | "permission_denied"
            | "away_summary" => Output::default(),
            other => Output::event(Event::Unrecognized {
                kind: format!("system/{other}"),
            }),
        }
    }

    /// Usage of the model call `message` came from, once per call and
    /// only as it grows: the token counts added since the call's last
    /// report, with no cost.
    fn call_usage(&mut self, message: &Value) -> Option<Event> {
        let turn = self.turns.active?;
        let id = message["id"].as_str()?;
        let usage = &message["usage"];
        if !usage.is_object() {
            return None;
        }
        let count = |field: &str| usage[field].as_u64().unwrap_or(0);
        let one_hour = usage["cache_creation"]["ephemeral_1h_input_tokens"]
            .as_u64()
            .unwrap_or(0);
        let now = Call {
            id: id.to_owned(),
            input: count("input_tokens"),
            output: count("output_tokens"),
            cache_read: count("cache_read_input_tokens"),
            cache_write: count("cache_creation_input_tokens").saturating_sub(one_hour),
            cache_write_1h: one_hour,
        };
        let before = match self.call.take() {
            Some(call) if call.id == now.id => call,
            _ => Call::default(),
        };
        let added = |now: u64, before: u64| Some(now.saturating_sub(before));
        let usage = Usage {
            cumulative: false,
            input_tokens: added(now.input, before.input),
            output_tokens: added(now.output, before.output),
            cached_input_tokens: added(now.cache_read, before.cache_read),
            cost_usd: None,
            cache_write_tokens: added(now.cache_write, before.cache_write),
            cache_write_1h_tokens: added(now.cache_write_1h, before.cache_write_1h),
            model: message["model"].as_str().map(str::to_owned),
        };
        let grew = [
            usage.input_tokens,
            usage.output_tokens,
            usage.cached_input_tokens,
            usage.cache_write_tokens,
            usage.cache_write_1h_tokens,
        ]
        .iter()
        .any(|n| n.unwrap_or(0) > 0);
        self.call = Some(now);
        grew.then_some(Event::UsageObserved {
            turn: Some(turn),
            usage,
        })
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
        let answering = message["user_message_uuid"].as_str();
        let mut events: Vec<_> = self.acknowledge(answering).into_iter().collect();
        let Some(turn) = self.turns.active else {
            return Output {
                events,
                frames: Vec::new(),
            };
        };
        // A turn the CLI runs of its own (`harness_turn`) says nothing in
        // this one, though its model calls still cost.
        let own = answering.is_none_or(|uuid| self.ours(uuid));
        for block in message["message"]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|_| own)
        {
            match block["type"].as_str() {
                Some("text") => {
                    let text = block["text"].as_str().unwrap_or_default();
                    // Each frame is a whole block, not a fragment of one:
                    // a block after a block is a new paragraph.
                    let text = match self.after_text {
                        true => format!("\n\n{text}"),
                        false => text.to_owned(),
                    };
                    self.after_text = true;
                    events.push(Event::MessageDelta { turn, text });
                }
                Some("tool_use") => {
                    self.after_text = false;
                    events.push(Event::ToolStarted {
                        turn,
                        call_id: block["id"].as_str().unwrap_or_default().to_owned(),
                        name: block["name"].as_str().unwrap_or_default().to_owned(),
                    });
                }
                _ => {}
            }
        }
        events.extend(self.call_usage(&message["message"]));
        Output {
            events,
            frames: Vec::new(),
        }
    }

    /// Whether `uuid`, a user message a frame answers, is the turn in
    /// flight's or one of its steered messages.
    fn ours(&self, uuid: &str) -> bool {
        self.turn_uuid.as_ref().is_some_and(|(own, _)| own == uuid)
            || self.steers.iter().any(|s| s.uuid == uuid)
    }

    fn result(&mut self, message: &Value) -> Output {
        let mut events = Vec::new();
        // Every message this result answered: several when steered
        // messages joined the turn. None named is a result from before
        // the CLI listed them, taken as the turn's.
        let answered: Option<Vec<&str>> = match message["user_message_uuids"].as_array() {
            Some(uuids) => Some(uuids.iter().filter_map(Value::as_str).collect()),
            None => message["user_message_uuid"].as_str().map(|uuid| vec![uuid]),
        };
        if let Some(answered) = answered
            .as_ref()
            .filter(|a| !a.iter().any(|u| self.ours(u)))
        {
            return Output {
                events: self.harness_turn(message, answered),
                frames: Vec::new(),
            };
        }
        let answered = answered.unwrap_or_default();
        self.steers.retain(|s| !answered.contains(&s.uuid.as_str()));
        if let (Some(turn), false) = (self.turns.active, self.steers.is_empty()) {
            // Queued steered messages run as a follow-up the CLI starts
            // itself: the turn goes on until a result answers them.
            events.push(Event::UsageObserved {
                turn: Some(turn),
                usage: usage(message),
            });
            match outcome(message, false) {
                TurnOutcome::Completed => {}
                other => events.push(Event::Warning {
                    message: format!(
                        "part of the turn ended {other:?}; it goes on with steered input"
                    ),
                }),
            }
            return Output {
                events,
                frames: Vec::new(),
            };
        }
        let Some(turn) = self.turns.end() else {
            return Output::event(Event::ProtocolViolation {
                detail: "result without a turn in flight".into(),
            });
        };
        self.turn_uuid = None;
        self.call = None;
        self.after_text = false;
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

    /// A `result` that answers none of the turn's messages: a turn the CLI
    /// ran of its own, such as the one Claude Code 2.1.293 runs on
    /// `--resume` for a notification queued in the earlier session (a
    /// stopped background task's `<task-notification>`), before the
    /// prompt. It is recorded and its cost counted; the turn in flight
    /// goes on until its own result.
    fn harness_turn(&mut self, message: &Value, answered: &[&str]) -> Vec<Event> {
        self.call = None;
        let answering = match answered {
            [] => "no message".to_owned(),
            uuids => uuids.join(", "),
        };
        vec![
            Event::UsageObserved {
                turn: self.turns.active,
                usage: usage(message),
            },
            Event::Warning {
                message: format!(
                    "Claude Code ran a turn of its own, answering {answering}, which ended {:?}; \
                     it is not the turn in flight",
                    outcome(message, false)
                ),
            },
        ]
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
        cache_write_tokens: sum("cacheCreationInputTokens"),
        ..Usage::default()
    }
}

/// A task as `task_started` and `background_tasks_changed` describe it.
fn task(value: &Value) -> HarnessTask {
    HarnessTask {
        task_id: value["task_id"].as_str().unwrap_or_default().to_owned(),
        kind: value["task_type"].as_str().map(str::to_owned),
        description: value["description"].as_str().unwrap_or_default().to_owned(),
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

/// `--mcp-config` JSON: `{"mcpServers": {<name>: <McpStdioServerConfig>}}`,
/// the value the Agent SDK passes for its `mcpServers` option, with HTTP and
/// SSE servers as `{"type": "http" | "sse", url, headers}`. This is also
/// the content of [`Open::mcp_config_file`].
pub fn mcp_config(servers: &[McpServer], remote: &[RemoteMcpServer]) -> String {
    let remote = remote.iter().map(|server| {
        let headers: serde_json::Map<String, Value> = server
            .headers
            .iter()
            .map(|(name, value)| (name.clone(), json!(value)))
            .collect();
        let config = json!({
            "type": server.transport.as_str(),
            "url": server.url,
            "headers": headers,
        });
        (server.name.clone(), config)
    });
    let servers: serde_json::Map<String, Value> = servers
        .iter()
        .map(|server| {
            let env: serde_json::Map<String, Value> = server
                .env
                .iter()
                .map(|(name, value)| (name.clone(), json!(value)))
                .collect();
            let config = json!({
                "type": "stdio",
                "command": server.command,
                "args": server.args,
                "env": env,
            });
            (server.name.clone(), config)
        })
        .chain(remote)
        .collect();
    json!({ "mcpServers": servers }).to_string()
}

/// A random version 4 UUID, hyphenated and lowercase: a fresh session ID
/// that another process cannot guess.
fn uuid_v4() -> String {
    uuid::Uuid::new_v4().to_string()
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
            steer: true,
            model: true,
            budget: true,
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
        // Claude Code stops the turn with `error_max_budget_usd` once the
        // process's own spend reaches it; a resumed session's earlier cost
        // does not count.
        match open.max_budget_usd {
            Some(usd) if !(usd.is_finite() && usd > 0.0) => {
                return Err(Rejected::InvalidOpen(format!(
                    "the spending limit must be a positive number of dollars, not {usd}"
                )));
            }
            Some(usd) => argv.extend(["--max-budget-usd".into(), usd.to_string()]),
            None => {}
        }
        crate::check_all_mcp_servers(&open)?;
        let secret_bearing = open.mcp_servers.iter().any(|s| !s.env.is_empty())
            || open
                .remote_mcp_servers
                .iter()
                .any(|s| !s.headers.is_empty());
        let any = !open.mcp_servers.is_empty() || !open.remote_mcp_servers.is_empty();
        match &open.mcp_config_file {
            Some(file) if !file.starts_with('/') => {
                return Err(Rejected::InvalidOpen(format!(
                    "the MCP configuration file must be an absolute path, not {file:?}"
                )));
            }
            Some(file) => argv.extend(["--mcp-config".into(), file.clone()]),
            None if secret_bearing => {
                return Err(Rejected::InvalidOpen(
                    "MCP server variables or headers would be readable by every process on the \
                     host in Claude Code's command line; give the servers in a file \
                     (Open::mcp_config_file)"
                        .into(),
                ));
            }
            None if any => {
                argv.extend([
                    "--mcp-config".into(),
                    mcp_config(&open.mcp_servers, &open.remote_mcp_servers),
                ]);
            }
            None => {}
        }
        match &open.instructions {
            Some(Instructions {
                plugin_dir: Some(dir),
                ..
            }) => argv.extend(["--plugin-dir".into(), dir.clone()]),
            Some(instructions) => {
                argv.extend(["--append-system-prompt".into(), instructions.text.clone()])
            }
            None => {}
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
            // Ignored on purpose: keep_alive is the transport's heartbeat,
            // which comes while the CLI waits as much as while it works,
            // so it is not progress; user echoes tool results the
            // assistant frames already account for; stream_event is
            // partial-message streaming, which this launch does not ask
            // for; active_goal and autocompact_state describe the
            // session's settings as it opens.
            "keep_alive" | "user" | "stream_event" | "active_goal" | "autocompact_state" => {
                Output::default()
            }
            // A tool call still running reports its elapsed time.
            "tool_progress" => Output::event(Event::Progress {
                turn: self.turns.active,
            }),
            // The account's rate limit: news only when it bites.
            "rate_limit_event" => {
                let info = &message["rate_limit_info"];
                match info["status"].as_str() {
                    None | Some("allowed") => Output::default(),
                    Some(status) => Output::event(Event::Warning {
                        message: format!(
                            "rate limit {status}{}",
                            info["rateLimitType"]
                                .as_str()
                                .map(|kind| format!(" ({kind})"))
                                .unwrap_or_default()
                        ),
                    }),
                }
            }
            "command_lifecycle" => {
                let state = message["state"].as_str().unwrap_or_default();
                let uuid = message["command_uuid"].as_str();
                let started = matches!(state, "queued" | "started");
                let mut events: Vec<Event> = started
                    .then(|| self.acknowledge(uuid))
                    .flatten()
                    .into_iter()
                    .collect();
                if let Some(uuid) = uuid {
                    events.extend(self.steer_lifecycle(uuid, state));
                }
                Output {
                    events,
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
        self.turn_uuid = Some((uuid.clone(), false));
        self.steers.clear();
        self.call = None;
        self.after_text = false;
        Ok(Submitted {
            turn,
            frames: vec![user_message(prompt, &uuid)],
        })
    }

    fn interrupt(&mut self) -> Result<Vec<Frame>, Rejected> {
        let turn = self.turns.active.ok_or(Rejected::NoTurn)?;
        self.interrupting = true;
        // Steered messages still queued would otherwise run after the
        // interrupt, as a turn of their own.
        let request = match self.steers.iter().any(|s| !s.started) {
            true => json!({"subtype": "interrupt", "cancel_queued": true}),
            false => json!({"subtype": "interrupt"}),
        };
        Ok(vec![self.control_request(Pending::Interrupt(turn), request)])
    }

    fn steer_boundary(&self) -> &'static str {
        // Queued as a stream-json `user` message, delivered before the next
        // model call (`docs/harness-integration.md` "Steering a running
        // turn").
        "claude_next_model_call"
    }

    fn steer(&mut self, text: &str) -> Result<Vec<Frame>, Rejected> {
        if !self.ready {
            return Err(Rejected::NotReady);
        }
        self.turns.active.ok_or(Rejected::NoTurn)?;
        let uuid = uuid_v4();
        self.next_steer += 1;
        self.steers.push(Steer {
            number: self.next_steer,
            uuid: uuid.clone(),
            accepted: false,
            started: false,
        });
        Ok(vec![user_message(text, &uuid)])
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

    fn close(&mut self) -> Vec<Frame> {
        if !self.ready {
            return Vec::new();
        }
        // Without it, the CLI outlives its input for as long as a
        // background task of its own runs.
        vec![self.control_request(Pending::EndSession, json!({"subtype": "end_session"}))]
    }

    fn transport_closed(&mut self) -> Vec<Event> {
        self.ready = false;
        self.permissions.clear();
        self.pending.clear();
        self.turn_uuid = None;
        self.steers.clear();
        self.turns.closed()
    }
}

/// A user message as stream-json input, stamped with `uuid`.
fn user_message(text: &str, uuid: &str) -> Frame {
    frame(&json!({
        "type": "user",
        "message": {"role": "user", "content": [{"type": "text", "text": text}]},
        "parent_tool_use_id": null,
        "session_id": "",
        "uuid": uuid,
    }))
}

#[cfg(test)]
mod tests {
    #[test]
    fn session_ids_are_distinct_hyphenated_version_4_uuids() {
        let a = super::uuid_v4();
        assert_eq!(a.len(), 36);
        let groups: Vec<usize> = a.split('-').map(str::len).collect();
        assert_eq!(groups, [8, 4, 4, 4, 12]);
        assert!(a
            .chars()
            .all(|c| c == '-' || c.is_ascii_digit() || ('a'..='f').contains(&c)));
        assert_eq!(&a[14..15], "4");
        assert!("89ab".contains(&a[19..20]), "{a}");
        assert_ne!(a, super::uuid_v4());
    }
}
