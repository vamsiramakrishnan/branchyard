//! Harness driver contract and protocol drivers.
//!
//! A [`Driver`] translates one harness wire protocol into Branchyard's
//! normalized [`Event`]s. Drivers are sans-IO: they build the launch argument
//! vector and the frames to write, and consume the lines the harness prints.
//! The caller owns the process, which runs through the sandbox provider's
//! exec with stdin/stdout as protocol pipes. Nothing here spawns a process,
//! touches the host filesystem or holds credentials.
//!
//! Three driver families cover the integration matrix's protocol profiles:
//!
//! - [`claude_code::ClaudeCode`]: Claude Code print mode with stream-json
//!   input and output and permission prompts over stdio.
//! - [`codex::Codex`]: Codex App Server JSON-RPC.
//! - [`acp::Acp`]: Agent Client Protocol v1, for every ACP profile.
//!
//! [`profiles`] maps harness IDs to a driver and launch command. A profile is
//! implemented, not qualified: support needs the runtime gates in
//! `docs/harness-integration.md`.

pub mod acp;
pub mod claude_code;
pub mod codex;
pub mod profiles;

use std::fmt;

pub use serde_json::Value;

/// A native session identifier, as the harness names it.
///
/// Values that could be read as command-line flags or carry control
/// characters are refused, because some drivers pass them as arguments.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NativeSession(String);

impl NativeSession {
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= 256
            && !value.starts_with('-')
            && !value.chars().any(|c| c.is_control() || c.is_whitespace());
        valid.then_some(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NativeSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// How a session begins. Exactly one mode; a driver never substitutes one
/// for another.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionMode {
    Fresh,
    /// Continue this native session under its existing identity.
    Resume(NativeSession),
    /// Continue from this native session under a new identity.
    Fork(NativeSession),
}

/// Parameters for opening a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Open {
    pub mode: SessionMode,
    /// Working directory inside the sandbox.
    pub cwd: String,
    /// Harness-specific model name, when the task pins one.
    pub model: Option<String>,
}

/// The process to start inside the sandbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchSpec {
    /// Argument vector, executed without a shell.
    pub argv: Vec<String>,
    pub cwd: String,
}

/// One newline-terminated protocol frame to write to the harness's stdin.
pub type Frame = Vec<u8>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opened {
    pub launch: LaunchSpec,
    /// Frames to write once the process has started.
    pub frames: Vec<Frame>,
}

/// A locally written turn. The sequence is local to one driver instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Submitted {
    pub turn: u64,
    pub frames: Vec<Frame>,
}

/// Events and response frames produced by one received line.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Output {
    pub events: Vec<Event>,
    pub frames: Vec<Frame>,
}

impl Output {
    fn event(event: Event) -> Self {
        Output {
            events: vec![event],
            frames: Vec::new(),
        }
    }
}

/// Identifies one outstanding permission request.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PermissionKey(pub String);

#[derive(Clone, Debug, PartialEq)]
pub struct PermissionRequest {
    pub key: PermissionKey,
    /// The harness's name for the tool or action.
    pub tool: String,
    /// The proposed invocation, as the harness reported it.
    pub input: Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Allow this one invocation.
    Allow,
    /// Deny this one invocation. Drivers pass the message on where the
    /// protocol carries one.
    Deny { message: String },
}

/// Token and cost observations. Fields a protocol does not report are `None`,
/// never zero.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    /// True when the values are running totals for the session rather than
    /// for one turn; consumers take deltas instead of summing.
    pub cumulative: bool,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    /// The harness's own cost estimate in USD, not a billing statement.
    pub cost_usd: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnOutcome {
    Completed,
    Interrupted,
    Failed {
        message: String,
    },
    /// Stopped by a limit such as maximum turns, tokens or budget.
    LimitReached {
        limit: String,
    },
    /// The model declined to continue.
    Refused,
}

/// Normalized driver events. Model stop, process exit and task acceptance
/// are different transitions and never share an event.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The protocol handshake completed.
    Ready,
    /// The native session is known. For a fork, `forked_from` names the
    /// parent and the session is guaranteed to differ from it.
    SessionStarted {
        session: NativeSession,
        forked_from: Option<NativeSession>,
    },
    /// Opening failed after launch; the session is unusable.
    OpenFailed {
        reason: String,
    },
    /// The harness acknowledged the turn before completing it.
    TurnAccepted {
        turn: u64,
        native: Option<String>,
    },
    MessageDelta {
        turn: u64,
        text: String,
    },
    ToolStarted {
        turn: u64,
        call_id: String,
        name: String,
    },
    PermissionRequested {
        turn: Option<u64>,
        request: PermissionRequest,
    },
    /// The harness withdrew a permission request it no longer needs.
    PermissionWithdrawn {
        key: PermissionKey,
    },
    UsageObserved {
        turn: Option<u64>,
        usage: Usage,
    },
    /// The harness confirmed it received an interrupt. The turn's terminal
    /// state still arrives as [`Event::TurnEnded`].
    InterruptAcknowledged {
        turn: u64,
    },
    TurnEnded {
        turn: u64,
        outcome: TurnOutcome,
    },
    /// The connection ended while a turn was in flight; whether it did work
    /// is unknown and must be reconciled before retrying.
    OutcomeUnknown {
        turn: u64,
        reason: String,
    },
    /// A request the profile does not implement. The driver has already
    /// answered it with an error.
    UnsupportedRequest {
        method: String,
    },
    /// A retry, configuration warning or other non-terminal notice.
    Warning {
        message: String,
    },
    /// The harness broke the protocol contract.
    ProtocolViolation {
        detail: String,
    },
    /// A well-formed message this driver does not interpret. Callers keep the
    /// raw line when they need it.
    Unrecognized {
        kind: String,
    },
    SessionClosed,
}

/// A local request the driver cannot carry out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rejected {
    /// The profile or negotiated protocol lacks this operation.
    Unsupported(String),
    /// The handshake has not completed.
    NotReady,
    /// A turn is already in flight.
    TurnInProgress,
    /// No turn is in flight.
    NoTurn,
    /// No such outstanding permission request.
    UnknownPermission,
    /// Opening was invalid, for example an empty working directory.
    InvalidOpen(String),
}

impl fmt::Display for Rejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Rejected::Unsupported(what) => write!(f, "unsupported: {what}"),
            Rejected::NotReady => f.write_str("the session is not ready"),
            Rejected::TurnInProgress => f.write_str("a turn is already in flight"),
            Rejected::NoTurn => f.write_str("no turn is in flight"),
            Rejected::UnknownPermission => f.write_str("no such permission request"),
            Rejected::InvalidOpen(why) => write!(f, "invalid open: {why}"),
        }
    }
}

impl std::error::Error for Rejected {}

/// What a driver profile can do.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub resume: bool,
    pub fork: bool,
    pub cancellation: bool,
    /// Per-invocation permission requests routed to Branchyard.
    pub tool_approvals: bool,
    /// Acknowledgment of a turn before it completes.
    pub turn_acknowledgment: bool,
    pub usage: bool,
}

/// What a task needs from its harness. Unset fields are not required.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Requirements {
    pub resume: bool,
    pub fork: bool,
    pub cancellation: bool,
    pub tool_approvals: bool,
    pub turn_acknowledgment: bool,
    pub usage: bool,
}

/// Admit `required` against `offered`, naming every unmet requirement.
///
/// Capabilities that depend on negotiation, such as ACP resume, are declared
/// as possible here and enforced again when the session opens.
pub fn admit(required: &Requirements, offered: &Capabilities) -> Result<(), Vec<&'static str>> {
    let missing: Vec<_> = [
        ("resume", required.resume, offered.resume),
        ("fork", required.fork, offered.fork),
        ("cancellation", required.cancellation, offered.cancellation),
        (
            "tool_approvals",
            required.tool_approvals,
            offered.tool_approvals,
        ),
        (
            "turn_acknowledgment",
            required.turn_acknowledgment,
            offered.turn_acknowledgment,
        ),
        ("usage", required.usage, offered.usage),
    ]
    .into_iter()
    .filter(|(_, needed, available)| *needed && !available)
    .map(|(name, _, _)| name)
    .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(missing)
    }
}

/// A harness wire protocol, driven without I/O.
pub trait Driver {
    /// Capabilities of this profile before negotiation.
    fn capabilities(&self) -> Capabilities;

    /// Build the launch and the frames that start the handshake.
    fn open(&mut self, open: Open) -> Result<Opened, Rejected>;

    /// Consume one line the harness wrote to stdout.
    fn receive(&mut self, line: &[u8]) -> Output;

    /// Write a user prompt as a new turn.
    fn submit(&mut self, prompt: &str) -> Result<Submitted, Rejected>;

    /// Request cancellation of the turn in flight.
    fn interrupt(&mut self) -> Result<Vec<Frame>, Rejected>;

    /// Answer an outstanding permission request.
    fn respond(
        &mut self,
        key: &PermissionKey,
        decision: PermissionDecision,
    ) -> Result<Vec<Frame>, Rejected>;

    /// The harness's stdout closed or the process exited.
    fn transport_closed(&mut self) -> Vec<Event>;
}

/// Serialize one frame as a single line.
pub(crate) fn frame(value: &Value) -> Frame {
    let mut bytes = serde_json::to_vec(value).expect("a JSON value always serializes");
    bytes.push(b'\n');
    bytes
}

/// Parse one received line, reporting non-JSON output as a violation.
pub(crate) fn parse(line: &[u8]) -> Result<Value, Output> {
    let text = String::from_utf8_lossy(line);
    let text = text.trim();
    if text.is_empty() {
        return Err(Output::default());
    }
    serde_json::from_str(text).map_err(|_| {
        Output::event(Event::ProtocolViolation {
            detail: "harness wrote a non-JSON line to its protocol stream".into(),
        })
    })
}

/// Turn bookkeeping shared by the drivers.
#[derive(Debug, Default)]
pub(crate) struct Turns {
    next: u64,
    pub(crate) active: Option<u64>,
}

impl Turns {
    pub(crate) fn begin(&mut self) -> Result<u64, Rejected> {
        if self.active.is_some() {
            return Err(Rejected::TurnInProgress);
        }
        self.next += 1;
        self.active = Some(self.next);
        Ok(self.next)
    }

    pub(crate) fn end(&mut self) -> Option<u64> {
        self.active.take()
    }

    pub(crate) fn closed(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        if let Some(turn) = self.end() {
            events.push(Event::OutcomeUnknown {
                turn,
                reason: "the harness connection closed before the turn ended".into(),
            });
        }
        events.push(Event::SessionClosed);
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_sessions_refuse_flag_and_control_shapes() {
        assert!(NativeSession::new("01a0dfdd-7f42").is_some());
        for bad in ["", "--resume", "a b", "a\nb", &"x".repeat(257)] {
            assert!(NativeSession::new(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn admission_names_every_missing_capability() {
        let offered = Capabilities {
            resume: true,
            ..Capabilities::default()
        };
        let required = Requirements {
            resume: true,
            fork: true,
            tool_approvals: true,
            ..Requirements::default()
        };
        assert_eq!(
            admit(&required, &offered),
            Err(vec!["fork", "tool_approvals"])
        );
        assert_eq!(admit(&Requirements::default(), &offered), Ok(()));
    }

    #[test]
    fn non_json_output_is_a_violation_and_blank_lines_are_ignored() {
        assert!(matches!(
            parse(b"Loading...\n").unwrap_err().events[..],
            [Event::ProtocolViolation { .. }]
        ));
        assert_eq!(parse(b"  \n").unwrap_err(), Output::default());
    }

    #[test]
    fn closing_mid_turn_reports_an_unknown_outcome() {
        let mut turns = Turns::default();
        let turn = turns.begin().unwrap();
        assert_eq!(turns.begin(), Err(Rejected::TurnInProgress));
        assert!(
            matches!(turns.closed()[..], [Event::OutcomeUnknown { turn: t, .. }, Event::SessionClosed] if t == turn)
        );
    }
}
