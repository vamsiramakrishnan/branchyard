//! Harness driver contract and protocol drivers.
//!
//! A [`Driver`] translates one harness wire protocol into Branchyard's
//! normalized [`Event`]s. Drivers are sans-IO: they build the launch argument
//! vector and the frames to write, and consume the lines the harness prints.
//! The caller owns the process, which runs through the sandbox provider's
//! exec with stdin/stdout as protocol pipes. Nothing here spawns a process,
//! touches the host filesystem or holds credentials.
//!
//! Six driver families cover the integration matrix's protocol profiles:
//!
//! - [`claude_code::ClaudeCode`]: Claude Code print mode with stream-json
//!   input and output and permission prompts over stdio.
//! - [`codex::Codex`]: Codex App Server JSON-RPC.
//! - [`acp::Acp`]: Agent Client Protocol v1, for every ACP profile.
//! - [`antigravity::Antigravity`], [`pi::Pi`] and [`amp::Amp`]: those
//!   harnesses' native streaming protocols. None routes tool approvals to
//!   Branchyard; see each module.
//!
//! [`profiles`] maps harness IDs to a driver and launch command. A profile is
//! implemented, not qualified: support needs the runtime gates in
//! `docs/harness-integration.md`.
//!
//! [`conformance`] holds the transcript replay and contract checks these
//! drivers are tested with, for authors of new drivers;
//! `docs/writing-a-driver.md` describes the process.

pub mod acp;
pub mod amp;
pub mod antigravity;
pub mod claude_code;
pub mod codex;
pub mod conformance;
pub mod pi;
pub mod profiles;

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
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

/// Serialized as the bare identifier string.
impl Serialize for NativeSession {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

/// Deserialization applies the same validation as [`NativeSession::new`].
impl<'de> Deserialize<'de> for NativeSession {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        NativeSession::new(value.clone()).ok_or_else(|| {
            serde::de::Error::custom(format!("{value:?} is not a usable native session ID"))
        })
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
    /// MCP servers the harness starts for this session, in its native
    /// configuration shape. Resumes and forks pass them again: harnesses do
    /// not keep them with the session.
    pub mcp_servers: Vec<McpServer>,
    /// Standing instructions that are not part of any prompt.
    pub instructions: Option<Instructions>,
    /// A file, as the harness sees it, already holding `mcp_servers` in the
    /// harness's configuration format, for a driver that would otherwise
    /// pass them on its command line, where every process on the host can
    /// read them: Claude Code's stream-json driver, whose file is
    /// [`claude_code::mcp_config`]. The caller writes it, readable only by
    /// the harness's user. Other drivers pass their servers over stdin and
    /// ignore it.
    pub mcp_config_file: Option<String>,
    /// HTTP and SSE MCP servers the harness connects to, for a driver that
    /// supports them (Claude Code's stream-json, and ACP agents that
    /// advertise them); every other driver refuses them.
    pub remote_mcp_servers: Vec<RemoteMcpServer>,
}

impl Open {
    /// A session in `cwd` with no model pinned and no MCP servers.
    pub fn new(mode: SessionMode, cwd: impl Into<String>) -> Open {
        Open {
            mode,
            cwd: cwd.into(),
            model: None,
            mcp_servers: Vec::new(),
            instructions: None,
            mcp_config_file: None,
            remote_mcp_servers: Vec::new(),
        }
    }
}

/// Standing instructions for a session, such as how to use Branchyard's
/// delegation tools. Each driver puts them where its harness reads
/// instructions, never in the working tree:
///
/// - Claude Code loads `plugin_dir` with `--plugin-dir`, so the text
///   arrives as a skill; without one, it appends `text` to the system
///   prompt.
/// - Codex sends `text` as the thread's `developerInstructions`.
/// - ACP has no instructions field; the first prompt the driver submits
///   starts with `text` between `<branchyard-instructions>` tags.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Instructions {
    pub text: String,
    /// A Claude Code plugin directory (`.claude-plugin/plugin.json` and
    /// `skills/<name>/SKILL.md`) carrying the same instructions.
    pub plugin_dir: Option<String>,
}

/// Opening tag of the ACP instructions preamble.
pub const PREAMBLE_OPEN: &str = "<branchyard-instructions>";
/// Closing tag of the ACP instructions preamble; the prompt follows after a
/// blank line.
pub const PREAMBLE_CLOSE: &str = "</branchyard-instructions>";

/// A stdio MCP server for the harness to start, such as Branchyard's own
/// tools. The harness launches it, so it runs with the harness's identity
/// and inside the harness's sandbox; nothing here grants it authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServer {
    /// The harness's name for the server: `[A-Za-z0-9_-]`, at most 64
    /// bytes. Claude Code names its tools `mcp__<name>__<tool>`.
    pub name: String,
    /// Executable. ACP requires an absolute path; the drivers require one
    /// everywhere so a server never resolves differently per harness.
    pub command: String,
    pub args: Vec<String>,
    /// Variables set for the server process, in addition to whatever the
    /// harness passes through.
    pub env: Vec<(String, String)>,
}

/// An MCP server the harness reaches over HTTP: MCP's streamable HTTP
/// transport, or the older SSE one. Header values often carry tokens, so
/// `Debug` shows only their names.
#[derive(Clone, PartialEq, Eq)]
pub struct RemoteMcpServer {
    /// As for [`McpServer::name`].
    pub name: String,
    pub transport: RemoteTransport,
    /// `http://` or `https://`.
    pub url: String,
    /// Sent with every request, such as `Authorization`.
    pub headers: Vec<(String, String)>,
}

impl std::fmt::Debug for RemoteMcpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteMcpServer")
            .field("name", &self.name)
            .field("transport", &self.transport)
            .field("url", &self.url)
            .field(
                "headers",
                &self
                    .headers
                    .iter()
                    .map(|(name, _)| format!("{name}: <redacted>"))
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// How a [`RemoteMcpServer`] is reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteTransport {
    /// Streamable HTTP.
    Http,
    Sse,
}

impl RemoteTransport {
    /// The name Claude Code's configuration and ACP use.
    pub fn as_str(self) -> &'static str {
        match self {
            RemoteTransport::Http => "http",
            RemoteTransport::Sse => "sse",
        }
    }
}

/// Refuse MCP servers and standing instructions for a driver that has no
/// verified way to pass them, rather than dropping them silently.
pub(crate) fn refuse_projection(open: &Open, harness: &str) -> Result<(), Rejected> {
    refuse_remote_mcp(open, harness)?;
    if !open.mcp_servers.is_empty() {
        return Err(Rejected::Unsupported(format!(
            "Branchyard cannot yet give {harness} MCP servers"
        )));
    }
    if open.instructions.is_some() {
        return Err(Rejected::Unsupported(format!(
            "Branchyard cannot yet give {harness} standing instructions"
        )));
    }
    Ok(())
}

/// Refuse HTTP and SSE MCP servers for a driver that cannot pass them.
pub(crate) fn refuse_remote_mcp(open: &Open, harness: &str) -> Result<(), Rejected> {
    match open.remote_mcp_servers.first() {
        Some(server) => Err(Rejected::Unsupported(format!(
            "Branchyard cannot yet give {harness} an HTTP or SSE MCP server ({})",
            server.name
        ))),
        None => Ok(()),
    }
}

/// [`check_mcp_servers`] for both of an open's lists, whose names must not
/// repeat across them, and the remote servers' URLs and headers.
pub(crate) fn check_all_mcp_servers(open: &Open) -> Result<(), Rejected> {
    check_mcp_servers(&open.mcp_servers)?;
    let invalid = |why: String| Err(Rejected::InvalidOpen(why));
    for (index, server) in open.remote_mcp_servers.iter().enumerate() {
        if !valid_server_name(&server.name) {
            return invalid(format!(
                "MCP server name {:?} is not [A-Za-z0-9_-]{{1,64}}",
                server.name
            ));
        }
        let repeated = open.mcp_servers.iter().any(|s| s.name == server.name)
            || open.remote_mcp_servers[..index]
                .iter()
                .any(|s| s.name == server.name);
        if repeated {
            return invalid(format!("MCP server {} is listed twice", server.name));
        }
        if let Err(why) = check_remote_url(&server.url) {
            return invalid(format!("MCP server {}: {why}", server.name));
        }
        for (name, value) in &server.headers {
            if let Err(why) = check_header(name, value) {
                return invalid(format!("MCP server {}: {why}", server.name));
            }
        }
    }
    Ok(())
}

/// An `http://` or `https://` URL with a host and no whitespace.
pub fn check_remote_url(url: &str) -> Result<(), String> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"));
    match rest {
        Some(rest)
            if !rest.is_empty()
                && !rest.starts_with('/')
                && !rest.contains(char::is_whitespace) =>
        {
            Ok(())
        }
        _ => Err(format!(
            "the URL must be http:// or https://HOST/..., not {url:?}"
        )),
    }
}

/// An HTTP header name (a token) and a value without line breaks.
pub fn check_header(name: &str, value: &str) -> Result<(), String> {
    let token = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b));
    if !token {
        return Err(format!("{name:?} is not an HTTP header name"));
    }
    if value.contains(['\r', '\n', '\0']) {
        return Err(format!("the value of header {name} spans lines"));
    }
    Ok(())
}

fn valid_server_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Reject server lists a harness could misread: bad or repeated names,
/// relative commands, or unusable variable names.
pub(crate) fn check_mcp_servers(servers: &[McpServer]) -> Result<(), Rejected> {
    let invalid = |why: String| Err(Rejected::InvalidOpen(why));
    for (index, server) in servers.iter().enumerate() {
        if !valid_server_name(&server.name) {
            return invalid(format!(
                "MCP server name {:?} is not [A-Za-z0-9_-]{{1,64}}",
                server.name
            ));
        }
        if servers[..index]
            .iter()
            .any(|other| other.name == server.name)
        {
            return invalid(format!("MCP server {} is listed twice", server.name));
        }
        if !server.command.starts_with('/') {
            return invalid(format!(
                "MCP server {} needs an absolute command, not {:?}",
                server.name, server.command
            ));
        }
        if let Some((var, _)) = server
            .env
            .iter()
            .find(|(var, _)| var.is_empty() || var.contains(['=', '\0']))
        {
            return invalid(format!(
                "MCP server {} sets an invalid variable {var:?}",
                server.name
            ));
        }
    }
    Ok(())
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
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PermissionKey(pub String);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub key: PermissionKey,
    /// The harness's name for the tool or action.
    pub tool: String,
    /// The proposed invocation, as the harness reported it.
    pub input: Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PermissionDecision {
    /// Allow this one invocation.
    Allow,
    /// Deny this one invocation. Drivers pass the message on where the
    /// protocol carries one.
    Deny { message: String },
}

/// Token and cost observations. Fields a protocol does not report are `None`,
/// never zero.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
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
///
/// Serialized as an object tagged by `type` in snake case, such as
/// `{"type": "turn_ended", "turn": 1, "outcome": {"kind": "completed"}}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
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
    /// The harness took steered input ([`Driver::steer`]) into the running
    /// turn. `steer` counts the driver's steers from 1, in call order. It
    /// says the harness will deliver the input within this turn, not that
    /// the model has read it yet; each driver documents when that happens.
    SteerAccepted {
        turn: u64,
        steer: u64,
    },
    /// The harness refused steered input, or dropped it undelivered: an
    /// interrupt cancelled it, or the turn ended first. The input never
    /// reached the model.
    SteerRejected {
        turn: u64,
        steer: u64,
        reason: String,
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
    /// The turn in flight cannot take steered input yet, for example
    /// because the harness has not acknowledged it; try again shortly.
    SteerNotYet,
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
            Rejected::SteerNotYet => {
                f.write_str("the turn in flight cannot take steered input yet")
            }
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
    /// Input delivered into a running turn ([`Driver::steer`]).
    pub steer: bool,
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
    pub steer: bool,
}

/// A capability name (as [`admit`] and [`Driver::capabilities`] name it,
/// e.g. `"fork"`) paired with why it is unsupported, or supported only with
/// a caveat. Quoted from the driver's own refusal message or module
/// documentation; `"not verified"` marks a gap where no such evidence exists
/// yet, never a guess. See `docs/compatibility.md`, which renders these.
pub type CapabilityReason = (&'static str, &'static str);

/// Look up why each name in `missing` (as [`admit`] returns it) is
/// unavailable, from a profile's [`Driver::capability_reasons`]. A name with
/// no recorded reason reports `"not verified"` rather than nothing, so a
/// caller never has to guess whether the gap is evidenced.
pub fn reasons_for(
    missing: &[&'static str],
    reasons: &[CapabilityReason],
) -> Vec<CapabilityReason> {
    missing
        .iter()
        .map(|name| {
            let reason = reasons
                .iter()
                .find(|(capability, _)| capability == name)
                .map_or("not verified", |(_, reason)| reason);
            (*name, reason)
        })
        .collect()
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
        ("steer", required.steer, offered.steer),
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

    /// Why each unsupported or partial capability in [`Driver::capabilities`]
    /// is the way it is (see [`CapabilityReason`]). A profile whose
    /// capabilities need no caveat returns `&[]`; the default does so for
    /// every driver that grants everything unconditionally.
    fn capability_reasons(&self) -> &'static [CapabilityReason] {
        &[]
    }

    /// Build the launch and the frames that start the handshake.
    fn open(&mut self, open: Open) -> Result<Opened, Rejected>;

    /// Consume one line the harness wrote to stdout.
    fn receive(&mut self, line: &[u8]) -> Output;

    /// Write a user prompt as a new turn.
    fn submit(&mut self, prompt: &str) -> Result<Submitted, Rejected>;

    /// Request cancellation of the turn in flight.
    fn interrupt(&mut self) -> Result<Vec<Frame>, Rejected>;

    /// Deliver `text` as user input into the turn in flight, without
    /// ending or interrupting it: the harness's own mid-turn input, never a
    /// silent interrupt-and-resubmit. The turn's number does not change,
    /// and it ends as usual with [`Event::TurnEnded`], once the harness has
    /// answered the steered input too. The harness confirms or refuses it
    /// with [`Event::SteerAccepted`] or [`Event::SteerRejected`], numbered
    /// by call order from 1.
    ///
    /// When the model sees the input differs by harness; each driver's
    /// module documents it. An interrupt also drops steered input the
    /// harness has not yet delivered.
    ///
    /// Rejected with [`Rejected::Unsupported`] and the reason when the
    /// profile cannot take input mid-turn (whatever its state, so a caller
    /// can ask before a turn runs), [`Rejected::NotReady`] before the
    /// handshake, [`Rejected::NoTurn`] with no turn in flight, and
    /// [`Rejected::SteerNotYet`] when the turn cannot take it yet. The
    /// default refuses, for drivers that do not implement it.
    fn steer(&mut self, text: &str) -> Result<Vec<Frame>, Rejected> {
        let _ = text;
        Err(Rejected::Unsupported(
            "this driver cannot deliver input into a running turn".into(),
        ))
    }

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

/// A JSON-RPC error's message with its `data`, which often holds the cause.
pub(crate) fn rpc_error(error: &Value) -> String {
    let message = error["message"].as_str().unwrap_or("error");
    match &error["data"] {
        Value::Null => message.to_owned(),
        Value::String(data) => format!("{message}: {data}"),
        data => match data["details"].as_str() {
            Some(details) => format!("{message}: {details}"),
            None => format!("{message}: {data}"),
        },
    }
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
    fn events_round_trip_through_json_and_sessions_stay_validated() {
        let events = [
            Event::Ready,
            Event::SessionStarted {
                session: NativeSession::new("s2").unwrap(),
                forked_from: NativeSession::new("s1"),
            },
            Event::PermissionRequested {
                turn: Some(1),
                request: PermissionRequest {
                    key: PermissionKey("7".into()),
                    tool: "Bash".into(),
                    input: serde_json::json!({"command": "ls"}),
                },
            },
            Event::UsageObserved {
                turn: None,
                usage: Usage {
                    cumulative: true,
                    cost_usd: Some(0.25),
                    ..Usage::default()
                },
            },
            Event::TurnEnded {
                turn: 1,
                outcome: TurnOutcome::Failed {
                    message: "m".into(),
                },
            },
            Event::SessionClosed,
        ];
        for event in &events {
            let text = serde_json::to_string(event).unwrap();
            assert_eq!(&serde_json::from_str::<Event>(&text).unwrap(), event);
        }
        assert_eq!(
            serde_json::to_value(&events[4]).unwrap(),
            serde_json::json!({"type": "turn_ended", "turn": 1, "outcome": {"kind": "failed", "message": "m"}})
        );
        let decision = PermissionDecision::Deny {
            message: "no".into(),
        };
        let text = serde_json::to_string(&decision).unwrap();
        assert_eq!(text, r#"{"decision":"deny","message":"no"}"#);
        assert!(serde_json::from_str::<NativeSession>(r#""--resume""#).is_err());
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
        let steering = Requirements {
            steer: true,
            ..Requirements::default()
        };
        assert_eq!(admit(&steering, &offered), Err(vec!["steer"]));
        assert_eq!(admit(&Requirements::default(), &offered), Ok(()));
    }

    #[test]
    fn admission_reasons_fall_back_to_not_verified() {
        let missing = admit(
            &Requirements {
                resume: true,
                fork: true,
                tool_approvals: true,
                ..Requirements::default()
            },
            &Capabilities {
                resume: true,
                ..Capabilities::default()
            },
        )
        .unwrap_err();
        let reasons = reasons_for(&missing, &[("fork", "the harness cannot fork a session")]);
        assert_eq!(
            reasons,
            vec![
                ("fork", "the harness cannot fork a session"),
                ("tool_approvals", "not verified"),
            ]
        );
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
