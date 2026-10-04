//! Conformance kit for driver authors.
//!
//! These helpers check a [`Driver`] against recorded harness transcripts and
//! against the contract every driver in this crate follows. They are test
//! tooling: like `assert_eq!`, they panic with a message naming the frame or
//! rule that failed, so they read naturally inside `#[test]` functions.
//!
//! A fixture transcript is JSON Lines. An optional first row
//! `{"note": "..."}` records provenance: harness version, launch and what was
//! redacted. Every other row is `{"dir": "out" | "in", "frame": {...}}`,
//! where `out` is a frame the client wrote to the harness and `in` is a line
//! the harness printed.
//!
//! ```no_run
//! use branchyard_harness::conformance::{Replay, Transcript};
//! use branchyard_harness::{codex::Codex, Driver, Open, SessionMode};
//!
//! let transcript = Transcript::load("tests/fixtures/codex-0.157.1-unauthenticated-turn.jsonl");
//! let mut driver = Codex::new(vec!["codex".into()]);
//! let opened = driver
//!     .open(Open::new(SessionMode::Fresh, "/workspace"))
//!     .unwrap();
//! let replayed = Replay::new(&transcript)
//!     .prompt("Say hello.")
//!     .run(&mut driver, &opened);
//! assert_eq!(replayed.sent, 4);
//! ```

#![allow(clippy::expect_used, clippy::panic)] // ratchet: branchyard-harness
use std::collections::VecDeque;
use std::path::Path;

use serde::de::DeserializeOwned;

use crate::{
    Driver, Event, Frame, Open, Opened, PermissionDecision, PermissionKey, Rejected, Value,
};

/// Which way a recorded frame travelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Printed by the harness; fed to [`Driver::receive`].
    In,
    /// Written by the client to the harness's stdin.
    Out,
}

/// One recorded frame.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub direction: Direction,
    pub frame: Value,
}

/// A recorded exchange between a client and a real harness.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Transcript {
    /// Provenance from the leading `{"note": ...}` row, if any.
    pub note: Option<String>,
    pub rows: Vec<Row>,
}

impl Transcript {
    /// Parse JSON Lines text. Blank lines are skipped; a note is accepted
    /// only as the first row, and any other row shape is an error.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut transcript = Transcript::default();
        for (index, line) in text.lines().enumerate() {
            let number = index + 1;
            if line.trim().is_empty() {
                continue;
            }
            let row: Value =
                serde_json::from_str(line).map_err(|e| format!("line {number}: {e}"))?;
            if let Some(note) = row.get("note") {
                if transcript.note.is_some() || !transcript.rows.is_empty() {
                    return Err(format!("line {number}: a note must be the first row"));
                }
                let note = note
                    .as_str()
                    .ok_or(format!("line {number}: the note is not a string"))?;
                transcript.note = Some(note.to_owned());
                continue;
            }
            let direction = match row["dir"].as_str() {
                Some("in") => Direction::In,
                Some("out") => Direction::Out,
                _ => return Err(format!("line {number}: \"dir\" must be \"in\" or \"out\"")),
            };
            let frame = row
                .get("frame")
                .ok_or(format!("line {number}: no \"frame\""))?
                .clone();
            transcript.rows.push(Row { direction, frame });
        }
        Ok(transcript)
    }

    /// Read and parse a transcript file.
    ///
    /// # Panics
    ///
    /// When the file cannot be read or does not parse.
    pub fn load(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        let text =
            std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        Self::parse(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }
}

/// Serialize `value` as one newline-terminated line, as a harness prints it.
pub fn line(value: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(value).expect("a JSON value always serializes");
    bytes.push(b'\n');
    bytes
}

/// Check a frame's shape: newline-terminated, no other newline, and exactly
/// one JSON value. Returns the value.
pub fn check_frame(frame: &[u8]) -> Result<Value, String> {
    if frame.last() != Some(&b'\n') {
        return Err("frame is not newline-terminated".into());
    }
    if frame.iter().filter(|b| **b == b'\n').count() != 1 {
        return Err("frame spans more than one line".into());
    }
    serde_json::from_slice(frame).map_err(|e| format!("frame is not one JSON value: {e}"))
}

/// Decode a frame the driver wrote, asserting its shape with [`check_frame`].
///
/// # Panics
///
/// When the frame is malformed.
pub fn decode(frame: &[u8]) -> Value {
    check_frame(frame).unwrap_or_else(|e| panic!("{e}: {:?}", String::from_utf8_lossy(frame)))
}

/// Decode every frame with [`decode`].
pub fn decode_all(frames: &[Frame]) -> Vec<Value> {
    frames.iter().map(|frame| decode(frame)).collect()
}

/// Assert that the part of `frame` at JSON `pointer` (`""` for the whole
/// frame, `"/params"` for its parameters) deserializes as `T`, typically a
/// type from the protocol's published schema crate.
///
/// # Panics
///
/// When nothing is at `pointer` or it does not deserialize as `T`.
pub fn assert_conforms<T: DeserializeOwned>(frame: &Value, pointer: &str) -> T {
    let part = frame
        .pointer(pointer)
        .unwrap_or_else(|| panic!("no {pointer:?} in {frame}"));
    serde_json::from_value(part.clone()).unwrap_or_else(|e| {
        panic!(
            "{pointer:?} does not conform to {}: {e}\n{frame}",
            std::any::type_name::<T>()
        )
    })
}

/// Feed one harness message and return its events and the decoded frames the
/// driver wrote in response, each shape-checked.
pub fn feed(driver: &mut dyn Driver, message: &Value) -> (Vec<Event>, Vec<Value>) {
    let output = driver.receive(&line(message));
    (output.events, decode_all(&output.frames))
}

/// Play the harness side of a handshake. Each frame the driver writes,
/// starting with `frames`, is passed to `answer`, which returns the messages
/// the harness would print; those are fed back until the driver writes no
/// more. Returns every event.
///
/// # Panics
///
/// After 64 frames, which means the handshake does not converge.
pub fn handshake(
    driver: &mut dyn Driver,
    frames: &[Frame],
    mut answer: impl FnMut(&Value) -> Vec<Value>,
) -> Vec<Event> {
    let mut queue: VecDeque<Value> = decode_all(frames).into();
    let mut events = Vec::new();
    let mut handled = 0;
    while let Some(frame) = queue.pop_front() {
        handled += 1;
        assert!(handled <= 64, "the handshake wrote more than 64 frames");
        for reply in answer(&frame) {
            let (received, written) = feed(driver, &reply);
            events.extend(received);
            queue.extend(written);
        }
    }
    events
}

/// Assert the rules every driver in this crate follows, against a scripted
/// exchange. `driver` must be unopened; it is opened with `open`, and
/// `answer` plays the harness during the handshake as in [`handshake`].
///
/// Checked, in order:
///
/// - submitting before [`Event::Ready`] is rejected with [`Rejected::NotReady`];
/// - the handshake produces [`Event::Ready`];
/// - a non-JSON line yields exactly one [`Event::ProtocolViolation`] and no
///   frame;
/// - answering an unknown permission key is [`Rejected::UnknownPermission`];
/// - interrupting with no turn in flight is [`Rejected::NoTurn`];
/// - a second submit while a turn is in flight is [`Rejected::TurnInProgress`];
/// - closing the transport mid-turn yields exactly [`Event::OutcomeUnknown`]
///   for that turn, then [`Event::SessionClosed`];
/// - submitting after the transport closed is rejected with
///   [`Rejected::NotReady`].
///
/// # Panics
///
/// On the first rule the driver breaks.
pub fn assert_contract(
    driver: &mut dyn Driver,
    open: Open,
    answer: impl FnMut(&Value) -> Vec<Value>,
) {
    assert_contract_greeted(driver, open, &[], answer);
}

/// [`assert_contract`] for a harness that speaks first: after the open,
/// `greeting` (the messages the harness prints unprompted, such as an
/// `init` line) is fed before the handshake frames are answered.
///
/// # Panics
///
/// On the first rule the driver breaks.
pub fn assert_contract_greeted(
    driver: &mut dyn Driver,
    open: Open,
    greeting: &[Value],
    answer: impl FnMut(&Value) -> Vec<Value>,
) {
    let opened = driver
        .open(open)
        .unwrap_or_else(|e| panic!("open rejected: {e}"));
    assert_eq!(
        driver.submit("before ready").err(),
        Some(Rejected::NotReady),
        "submitting before the handshake completes must be rejected"
    );
    let mut frames = opened.frames.clone();
    let mut events = Vec::new();
    for message in greeting {
        let output = driver.receive(&line(message));
        events.extend(output.events);
        frames.extend(output.frames);
    }
    events.extend(handshake(driver, &frames, answer));
    assert!(
        events.contains(&Event::Ready),
        "the handshake did not produce Ready: {events:?}"
    );

    let output = driver.receive(b"Loading...\n");
    assert!(
        matches!(output.events[..], [Event::ProtocolViolation { .. }]) && output.frames.is_empty(),
        "a non-JSON line must be one ProtocolViolation and no frame: {output:?}"
    );
    assert_eq!(
        driver.respond(
            &PermissionKey("branchyard-conformance-unknown".into()),
            PermissionDecision::Allow
        ),
        Err(Rejected::UnknownPermission),
        "answering an unknown permission key must be rejected"
    );
    assert_eq!(
        driver.interrupt(),
        Err(Rejected::NoTurn),
        "interrupting with no turn in flight must be rejected"
    );

    let submitted = driver
        .submit("first")
        .unwrap_or_else(|e| panic!("submit after Ready rejected: {e}"));
    decode_all(&submitted.frames);
    assert_eq!(
        driver.submit("second").err(),
        Some(Rejected::TurnInProgress),
        "only one turn may be in flight"
    );
    let closed = driver.transport_closed();
    assert!(
        matches!(
            closed[..],
            [Event::OutcomeUnknown { turn, .. }, Event::SessionClosed] if turn == submitted.turn
        ),
        "closing mid-turn must yield OutcomeUnknown for turn {} then SessionClosed: {closed:?}",
        submitted.turn
    );
    assert_eq!(
        driver.submit("after close").err(),
        Some(Rejected::NotReady),
        "submitting after the transport closed must be rejected"
    );
}

/// Assert the steering rules ([`Driver::steer`]) against a scripted
/// exchange. `driver` must be unopened; it is opened with `open`, fed
/// `greeting` as in [`assert_contract_greeted`] (empty for a harness that
/// does not speak first), and `answer` plays the harness for the handshake
/// and then for the frames of one submitted turn, as in [`handshake`], so a
/// driver that needs the harness to acknowledge a turn before steering it
/// gets that answer.
///
/// For a driver whose [`Capabilities::steer`](crate::Capabilities) is false,
/// every steer, before and after the handshake and mid-turn, must be
/// [`Rejected::Unsupported`] with a reason. Otherwise, checked in order:
///
/// - steering before [`Event::Ready`] is [`Rejected::NotReady`];
/// - steering with no turn in flight is [`Rejected::NoTurn`];
/// - steering the submitted turn writes at least one well-formed frame and
///   leaves the turn in flight: a second submit is still
///   [`Rejected::TurnInProgress`];
/// - closing the transport then yields [`Event::OutcomeUnknown`] for that
///   turn and [`Event::SessionClosed`], and steering after the close is
///   [`Rejected::NotReady`].
///
/// # Panics
///
/// On the first rule the driver breaks.
pub fn assert_steer_contract(
    driver: &mut dyn Driver,
    open: Open,
    greeting: &[Value],
    mut answer: impl FnMut(&Value) -> Vec<Value>,
) {
    let steers = driver.capabilities().steer;
    let unsupported = |result: Result<Vec<Frame>, Rejected>, when: &str| match result {
        Err(Rejected::Unsupported(reason)) if !reason.is_empty() => {}
        other => panic!(
            "a driver without steering must refuse it {when} as Unsupported with a reason, \
             not {other:?}"
        ),
    };
    if !steers {
        unsupported(driver.steer("unopened"), "before open");
    }
    let opened = driver
        .open(open)
        .unwrap_or_else(|e| panic!("open rejected: {e}"));
    match steers {
        true => assert_eq!(
            driver.steer("before ready").err(),
            Some(Rejected::NotReady),
            "steering before the handshake completes must be rejected"
        ),
        false => unsupported(driver.steer("before ready"), "before the handshake"),
    }
    let mut frames = opened.frames.clone();
    let mut events = Vec::new();
    for message in greeting {
        let output = driver.receive(&line(message));
        events.extend(output.events);
        frames.extend(output.frames);
    }
    events.extend(handshake(driver, &frames, &mut answer));
    assert!(
        events.contains(&Event::Ready),
        "the handshake did not produce Ready: {events:?}"
    );
    match steers {
        true => assert_eq!(
            driver.steer("no turn").err(),
            Some(Rejected::NoTurn),
            "steering with no turn in flight must be rejected"
        ),
        false => unsupported(driver.steer("no turn"), "with no turn"),
    }
    let submitted = driver
        .submit("first")
        .unwrap_or_else(|e| panic!("submit after Ready rejected: {e}"));
    handshake(driver, &submitted.frames, &mut answer);
    if !steers {
        unsupported(driver.steer("mid-turn"), "mid-turn");
        return;
    }
    let frames = driver
        .steer("mid-turn")
        .unwrap_or_else(|e| panic!("steering the turn in flight rejected: {e}"));
    assert!(!frames.is_empty(), "a steer must write a frame");
    decode_all(&frames);
    assert_eq!(
        driver.submit("second").err(),
        Some(Rejected::TurnInProgress),
        "a steer must not end the turn in flight"
    );
    let closed = driver.transport_closed();
    assert!(
        closed
            .iter()
            .any(|e| matches!(e, Event::OutcomeUnknown { turn, .. } if *turn == submitted.turn))
            && closed.last() == Some(&Event::SessionClosed),
        "closing a steered turn must yield OutcomeUnknown for turn {} and SessionClosed: \
         {closed:?}",
        submitted.turn
    );
    assert_eq!(
        driver.steer("after close").err(),
        Some(Rejected::NotReady),
        "steering after the transport closed must be rejected"
    );
}

/// Input a [`Replay`] writes on the client's initiative.
#[derive(Clone, Debug)]
enum Input {
    Prompt(String),
    Steer(String),
    Interrupt,
}

/// Replays a [`Transcript`] through a driver.
///
/// Recorded `in` frames are fed to [`Driver::receive`]. Each recorded `out`
/// frame is compared with the next frame the driver wrote; when the driver
/// has written nothing pending, the next queued [`prompt`](Self::prompt),
/// [`steer`](Self::steer) or [`interrupt`](Self::interrupt) is carried out
/// first, because those are the frames a driver writes on its own
/// initiative rather than in response to output.
///
/// Fields that legitimately differ between runs are declared with
/// [`alias`](Self::alias) or [`ignore`](Self::ignore).
#[derive(Debug)]
pub struct Replay<'t> {
    transcript: &'t Transcript,
    inputs: VecDeque<Input>,
    aliases: Vec<String>,
    ignored: Vec<String>,
    permissions: Option<PermissionDecision>,
}

/// The result of [`Replay::run`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Replayed {
    /// Every event, in order.
    pub events: Vec<Event>,
    /// Recorded outgoing frames the driver reproduced.
    pub sent: usize,
    /// Frames the driver wrote after the last recorded outgoing frame.
    pub unsent: Vec<Value>,
    /// Learned `(recorded, driver)` value pairs for aliased fields.
    pub aliases: Vec<(Value, Value)>,
}

impl Replayed {
    /// The driver's value for a recorded aliased value.
    pub fn ours(&self, recorded: &Value) -> Option<&Value> {
        self.aliases
            .iter()
            .find(|(theirs, _)| theirs == recorded)
            .map(|(_, ours)| ours)
    }
}

impl<'t> Replay<'t> {
    pub fn new(transcript: &'t Transcript) -> Self {
        Self {
            transcript,
            inputs: VecDeque::new(),
            aliases: Vec::new(),
            ignored: Vec::new(),
            permissions: None,
        }
    }

    /// Queue a prompt to submit when a recorded outgoing frame has no pending
    /// driver frame to match. Prompts and steers are used in order.
    pub fn prompt(mut self, text: impl Into<String>) -> Self {
        self.inputs.push_back(Input::Prompt(text.into()));
        self
    }

    /// Queue input to [`Driver::steer`] into the running turn, in order
    /// with the prompts, when a recorded outgoing frame has no pending
    /// driver frame to match.
    pub fn steer(mut self, text: impl Into<String>) -> Self {
        self.inputs.push_back(Input::Steer(text.into()));
        self
    }

    /// Queue a [`Driver::interrupt`], in order with the prompts and steers.
    pub fn interrupt(mut self) -> Self {
        self.inputs.push_back(Input::Interrupt);
        self
    }

    /// A field, by JSON pointer into outgoing frames, whose value the driver
    /// chooses per run, such as a request ID or a random UUID. When the
    /// driver's value differs from the recorded one, the pair is learned and
    /// the fields compare equal. In later incoming frames a learned string
    /// is replaced by the driver's wherever it appears; other values, such
    /// as numeric JSON-RPC IDs, only at the same pointer.
    pub fn alias(mut self, pointer: impl Into<String>) -> Self {
        self.aliases.push(pointer.into());
        self
    }

    /// A field, by JSON pointer into outgoing frames, whose value is not
    /// compared, such as a client version. It must still be present in both
    /// frames or neither.
    pub fn ignore(mut self, pointer: impl Into<String>) -> Self {
        self.ignored.push(pointer.into());
        self
    }

    /// Answer every permission request the replay raises with `decision`, so
    /// a recorded answer can be compared with the driver's.
    pub fn answer_permissions(mut self, decision: PermissionDecision) -> Self {
        self.permissions = Some(decision);
        self
    }

    /// Replay the transcript through `driver`, which `opened` came from.
    ///
    /// # Panics
    ///
    /// When an outgoing frame differs from the recorded one, the driver
    /// writes nothing where the transcript records a frame, a submit, steer
    /// or interrupt is rejected, or any frame is malformed.
    pub fn run(mut self, driver: &mut dyn Driver, opened: &Opened) -> Replayed {
        let mut written: VecDeque<Value> = decode_all(&opened.frames).into();
        let mut replayed = Replayed::default();
        for (index, row) in self.transcript.rows.iter().enumerate() {
            match row.direction {
                Direction::Out => {
                    if written.is_empty() {
                        match self.inputs.pop_front() {
                            Some(Input::Prompt(prompt)) => {
                                let submitted = driver.submit(&prompt).unwrap_or_else(|e| {
                                    panic!("row {index}: submit rejected: {e}")
                                });
                                written.extend(decode_all(&submitted.frames));
                            }
                            Some(Input::Steer(text)) => {
                                let frames = driver
                                    .steer(&text)
                                    .unwrap_or_else(|e| panic!("row {index}: steer rejected: {e}"));
                                written.extend(decode_all(&frames));
                            }
                            Some(Input::Interrupt) => {
                                let frames = driver.interrupt().unwrap_or_else(|e| {
                                    panic!("row {index}: interrupt rejected: {e}")
                                });
                                written.extend(decode_all(&frames));
                            }
                            None => {}
                        }
                    }
                    let Some(mut ours) = written.pop_front() else {
                        panic!(
                            "row {index}: the transcript records an outgoing frame the driver \
                             did not write, and no input is queued: {}",
                            row.frame
                        );
                    };
                    let mut recorded = row.frame.clone();
                    self.normalize(&mut ours, &mut recorded, &mut replayed.aliases);
                    assert!(
                        ours == recorded,
                        "row {index}: outgoing frame {} differs\n  driver:   {ours}\n  recorded: {recorded}",
                        replayed.sent
                    );
                    replayed.sent += 1;
                }
                Direction::In => {
                    let mut message = row.frame.clone();
                    self.substitute(&mut message, &replayed.aliases);
                    let (events, frames) = feed(driver, &message);
                    written.extend(frames);
                    for event in &events {
                        if let (Some(decision), Event::PermissionRequested { request, .. }) =
                            (&self.permissions, event)
                        {
                            let frames = driver
                                .respond(&request.key, decision.clone())
                                .unwrap_or_else(|e| panic!("row {index}: answer rejected: {e}"));
                            written.extend(decode_all(&frames));
                        }
                    }
                    replayed.events.extend(events);
                }
            }
        }
        replayed.unsent = written.into();
        replayed
    }

    fn normalize(&self, ours: &mut Value, recorded: &mut Value, learned: &mut Vec<(Value, Value)>) {
        for pointer in &self.ignored {
            if let (Some(a), Some(b)) = (ours.pointer_mut(pointer), recorded.pointer_mut(pointer)) {
                *a = Value::Null;
                *b = Value::Null;
            }
        }
        for pointer in &self.aliases {
            let (Some(a), Some(b)) = (ours.pointer_mut(pointer), recorded.pointer(pointer)) else {
                continue;
            };
            if a == b {
                continue;
            }
            match learned.iter().find(|(theirs, _)| theirs == b) {
                // The same recorded value must map to the same driver value.
                Some((_, known)) if known != a => continue,
                Some(_) => {}
                None => learned.push((b.clone(), a.clone())),
            }
            *a = b.clone();
        }
    }

    fn substitute(&self, message: &mut Value, learned: &[(Value, Value)]) {
        replace_strings(message, learned);
        for pointer in &self.aliases {
            if let Some(value) = message.pointer_mut(pointer) {
                if let Some((_, ours)) = learned.iter().find(|(theirs, _)| theirs == value) {
                    *value = ours.clone();
                }
            }
        }
    }
}

fn replace_strings(value: &mut Value, learned: &[(Value, Value)]) {
    match value {
        Value::String(_) => {
            if let Some((_, ours)) = learned.iter().find(|(theirs, _)| theirs == value) {
                *value = ours.clone();
            }
        }
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| replace_strings(item, learned)),
        Value::Object(fields) => fields
            .values_mut()
            .for_each(|field| replace_strings(field, learned)),
        _ => {}
    }
}
