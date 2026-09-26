//! Shared helpers for replaying recorded harness transcripts.

#![allow(dead_code)]

use branchyard_harness::{Driver, Event, Frame, Value};

/// One recorded frame: `out` was written to the harness, `in` was read.
pub struct Recorded {
    pub outgoing: bool,
    pub frame: Value,
}

/// Load a fixture transcript, skipping its provenance note.
pub fn transcript(name: &str) -> Vec<Recorded> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{path}: {e}"))
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row.get("frame").is_some())
        .map(|row| Recorded {
            outgoing: row["dir"] == "out",
            frame: row["frame"].clone(),
        })
        .collect()
}

pub fn line(value: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(value).unwrap();
    bytes.push(b'\n');
    bytes
}

pub fn decode(frame: &Frame) -> Value {
    assert_eq!(frame.last(), Some(&b'\n'), "frames are newline-terminated");
    assert_eq!(
        frame.iter().filter(|b| **b == b'\n').count(),
        1,
        "one frame per line"
    );
    serde_json::from_slice(frame).unwrap()
}

/// Feed one message and return its events and decoded response frames.
pub fn feed(driver: &mut dyn Driver, message: &Value) -> (Vec<Event>, Vec<Value>) {
    let output = driver.receive(&line(message));
    (output.events, output.frames.iter().map(decode).collect())
}
