//! Gemini through its OpenAI-compatible endpoint
//! (`https://generativelanguage.googleapis.com/v1beta/openai`), for a
//! harness that speaks OpenAI's chat completions and for a Claude request
//! translated onto them (`docs/model-gateway.md`, "Gemini through the
//! OpenAI-compatible endpoint"). A backend with [`Compat::Gemini`] (set as
//! `compat = "gemini"`, or by its URL's host) gets:
//!
//! - **Its path.** The endpoint's base URL stands for OpenAI's `/v1`, so a
//!   path's leading `/v1` is dropped ([`path`]): `/v1/chat/completions` goes
//!   to `…/v1beta/openai/chat/completions`.
//! - **Fields it rejects, stripped** ([`UNSUPPORTED`]), from a chat
//!   completion.
//! - **Thought signatures.** Gemini 3 signs the first tool call of each step
//!   with an encrypted record of its reasoning
//!   (`tool_calls[].extra_content.google.thought_signature`, in a whole
//!   answer and in a stream's deltas) and refuses, 400, a request that
//!   replays a step without it. A harness that speaks OpenAI's API drops
//!   the field, so the gateway keeps each signature by its call's id as the
//!   answer passes ([`Recorder`] into [`Signatures`]), and puts it back on
//!   the call when a request replays it ([`prepare`]). A step whose first
//!   call has none recorded (a conversation begun elsewhere, or a call
//!   that came back without an id) gets Google's documented placeholder,
//!   [`PLACEHOLDER`].
//!
//! Signatures are kept in this process, by the person the turn acts for and
//! the call's id, at most [`MAX_ENTRIES`] of them, the least recently used
//! forgotten first. Nothing here logs a signature: [`log`] names the fields
//! stripped and counts the signatures put back.
//!
//! [`Compat::Gemini`]: super::Compat::Gemini

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::Mutex;

use branchyard_support::LockExt as _;
use serde_json::{Map, Value};

use super::usage::{Events, MAX_BODY};

/// The host of Gemini's API, which turns the layer on for an `openai`
/// backend that does not say otherwise.
const HOST: &str = "generativelanguage.googleapis.com";

/// What a step's first call is given when no signature of it was recorded:
/// the value Google documents for "function calls that were not generated
/// by the API", which the endpoint accepts (measured 11 October 2026) where
/// a call with no signature is refused. It costs the model the reasoning
/// the real one carried. See
/// <https://ai.google.dev/gemini-api/docs/thought-signatures> (now part of
/// <https://ai.google.dev/gemini-api/docs/thinking>) and
/// <https://cloud.google.com/vertex-ai/generative-ai/docs/thought-signatures>.
pub(crate) const PLACEHOLDER: &str = "skip_thought_signature_validator";

/// Request fields stripped from a chat completion for Gemini's endpoint.
///
/// The endpoint parses a request into its own message types and refuses a
/// name it does not know: `store` is answered 400 `Unknown name "store":
/// Cannot find field.` (measured 11 October 2026; DeepSeek Harness sends
/// `"store": false`). Google's page for the endpoint
/// (<https://ai.google.dev/gemini-api/docs/openai>) lists no parameters,
/// and the list of those its Vertex AI counterpart supports
/// (<https://cloud.google.com/vertex-ai/generative-ai/docs/migrate/openai/overview>,
/// "Supported parameters") names none of the others here. They are OpenAI's
/// own platform features: stored completions and their metadata, predicted
/// outputs, prompt-cache routing, and the end-user and safety identifiers
/// (a translated Claude request carries `user`, from `metadata.user_id`).
/// Gemini has nothing to act on them with, so stripping them loses nothing.
/// Fields Gemini could act on (`parallel_tool_calls`, `logprobs`, ...) are
/// left as they are.
pub(crate) const UNSUPPORTED: &[&str] = &[
    "store",
    "metadata",
    "prediction",
    "prompt_cache_key",
    "safety_identifier",
    "user",
];

/// The most signatures kept, and the most bytes they and their keys may
/// take together; past either, the least recently used are forgotten.
pub(crate) const MAX_ENTRIES: usize = 4096;
const MAX_BYTES: usize = 32 * 1024 * 1024;

/// Whether `host` is Gemini's API.
pub(crate) fn is_host(host: &str) -> bool {
    host.eq_ignore_ascii_case(HOST)
}

/// Whether `path` is OpenAI's chat completions.
pub(crate) fn is_chat(path: &str) -> bool {
    path.split('?')
        .next()
        .unwrap_or("")
        .ends_with("/chat/completions")
}

/// The path to send Gemini's endpoint for OpenAI's `path`: its leading
/// `/v1` dropped, since the endpoint's base URL (`…/v1beta/openai`) stands
/// for it.
pub(crate) fn path(path: &str) -> String {
    match path.strip_prefix("/v1") {
        Some(rest) if rest.is_empty() || rest.starts_with(['/', '?']) => rest.to_owned(),
        _ => path.to_owned(),
    }
}

/// The thought signatures Gemini put on tool calls, by the person the turn
/// acts for and the call's id: a bounded map, the least recently used entry
/// forgotten first. One per yard's gateway, shared by its turns in this
/// process, so a conversation keeps its signatures across turns.
pub(crate) struct Signatures {
    kept: Mutex<Kept>,
    max_entries: usize,
    max_bytes: usize,
}

#[derive(Default)]
struct Kept {
    /// Key to signature and its last use.
    by_key: HashMap<String, (String, u64)>,
    /// Last use to key, oldest first.
    by_use: BTreeMap<u64, String>,
    tick: u64,
    bytes: usize,
}

impl Default for Signatures {
    fn default() -> Signatures {
        Signatures::bounded(MAX_ENTRIES, MAX_BYTES)
    }
}

impl Signatures {
    /// At most `entries` signatures, taking at most `bytes` with their keys.
    pub(crate) fn bounded(entries: usize, bytes: usize) -> Signatures {
        Signatures {
            kept: Mutex::new(Kept::default()),
            max_entries: entries,
            max_bytes: bytes,
        }
    }

    fn key(scope: &str, id: &str) -> String {
        format!("{scope}\n{id}")
    }

    /// Keep `signature` for call `id` of `scope`'s, as the most recently
    /// used. One too large to keep within the limit is not kept, nor is
    /// what was kept for the call before.
    pub(crate) fn put(&self, scope: &str, id: &str, signature: &str) {
        let key = Signatures::key(scope, id);
        let size = key.len() + signature.len();
        let mut kept = self.kept.lock_recovering("thought signatures");
        let kept = &mut *kept;
        if let Some((old, used)) = kept.by_key.remove(&key) {
            kept.by_use.remove(&used);
            kept.bytes -= key.len() + old.len();
        }
        if size > self.max_bytes {
            return;
        }
        kept.tick += 1;
        kept.by_use.insert(kept.tick, key.clone());
        kept.by_key.insert(key, (signature.to_owned(), kept.tick));
        kept.bytes += size;
        while kept.by_key.len() > self.max_entries || kept.bytes > self.max_bytes {
            let Some((_, oldest)) = kept.by_use.pop_first() else {
                break;
            };
            if let Some((old, _)) = kept.by_key.remove(&oldest) {
                kept.bytes -= oldest.len() + old.len();
            }
        }
    }

    /// The signature kept for call `id` of `scope`'s, now the most recently
    /// used.
    pub(crate) fn get(&self, scope: &str, id: &str) -> Option<String> {
        let key = Signatures::key(scope, id);
        let mut kept = self.kept.lock_recovering("thought signatures");
        let kept = &mut *kept;
        let (signature, used) = kept.by_key.get_mut(&key)?;
        kept.tick += 1;
        kept.by_use.remove(used);
        kept.by_use.insert(kept.tick, key);
        *used = kept.tick;
        Some(signature.clone())
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.kept.lock_recovering("thought signatures").by_key.len()
    }
}

/// A tool call's thought signature, if it carries one.
fn signature(call: &Value) -> Option<&str> {
    call.get("extra_content")?
        .get("google")?
        .get("thought_signature")?
        .as_str()
        .filter(|s| !s.is_empty())
}

/// Put `signature` on `call`, where Gemini's endpoint reads it, keeping
/// whatever else its `extra_content` holds.
fn sign(call: &mut Map<String, Value>, signature: String) {
    let extra = call
        .entry("extra_content")
        .or_insert_with(|| Value::Object(Map::new()));
    if !extra.is_object() {
        *extra = Value::Object(Map::new());
    }
    let Some(extra) = extra.as_object_mut() else {
        return;
    };
    let google = extra
        .entry("google")
        .or_insert_with(|| Value::Object(Map::new()));
    if !google.is_object() {
        *google = Value::Object(Map::new());
    }
    if let Some(google) = google.as_object_mut() {
        google.insert("thought_signature".into(), Value::String(signature));
    }
}

/// What [`prepare`] changed in a request: names and counts, never a
/// signature.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Prepared {
    /// The fields stripped, in [`UNSUPPORTED`]'s order.
    pub stripped: Vec<&'static str>,
    /// Calls given the signature recorded for them.
    pub recorded: usize,
    /// Calls given the [`PLACEHOLDER`].
    pub placeholders: usize,
}

impl Prepared {
    pub(crate) fn changed(&self) -> bool {
        !self.stripped.is_empty() || self.recorded > 0 || self.placeholders > 0
    }
}

impl fmt::Display for Prepared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        if !self.stripped.is_empty() {
            parts.push(format!("stripped {}", self.stripped.join(", ")));
        }
        if self.recorded > 0 || self.placeholders > 0 {
            parts.push(format!(
                "thought signatures attached: {} recorded, {} placeholder",
                self.recorded, self.placeholders
            ));
        }
        match parts.is_empty() {
            true => f.write_str("nothing changed"),
            false => f.write_str(&parts.join("; ")),
        }
    }
}

/// Make chat completions `request` one Gemini's endpoint takes: the
/// [`UNSUPPORTED`] fields stripped, and every replayed tool call that lacks
/// a signature given the one recorded for its id among `scope`'s, else, when
/// it is the first call of its step (the one Gemini signs and checks), the
/// [`PLACEHOLDER`]. A call that carries a signature is left as it is.
pub(crate) fn prepare(request: &mut Value, signatures: &Signatures, scope: &str) -> Prepared {
    let mut prepared = Prepared::default();
    let Some(object) = request.as_object_mut() else {
        return prepared;
    };
    for name in UNSUPPORTED {
        if object.remove(*name).is_some() {
            prepared.stripped.push(name);
        }
    }
    let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut) else {
        return prepared;
    };
    for message in messages {
        let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) else {
            continue;
        };
        for (n, call) in calls.iter_mut().enumerate() {
            if signature(call).is_some() {
                continue;
            }
            let recorded = call
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .and_then(|id| signatures.get(scope, id));
            let Some(call) = call.as_object_mut() else {
                continue;
            };
            match recorded {
                Some(found) => {
                    sign(call, found);
                    prepared.recorded += 1;
                }
                None if n == 0 => {
                    sign(call, PLACEHOLDER.to_owned());
                    prepared.placeholders += 1;
                }
                None => {}
            }
        }
    }
    prepared
}

/// Say at debug level what [`prepare`] changed in a call to `backend` for
/// `model`, when it changed anything.
pub(crate) fn log(backend: &str, model: &str, prepared: &Prepared) {
    if prepared.changed() {
        tracing::debug!(backend, model, "gemini compatibility: {prepared}");
    }
}

/// A stream's tool call, by choice and call index, as its deltas arrive.
#[derive(Default)]
struct Call {
    id: Option<String>,
    signature: Option<String>,
    kept: bool,
}

/// Reads the thought signatures off a Gemini backend's chat completion as
/// its bytes pass, and keeps each in [`Signatures`] by its call's id. A
/// stream's are kept as each event completes, before the harness can have
/// read the end of the answer and replayed the call; a whole answer's when
/// it ends, which the gateway reads before the harness gets the answer's
/// last chunk. A call without an id cannot be kept.
pub(crate) struct Recorder<'a> {
    signatures: &'a Signatures,
    scope: &'a str,
    sse: bool,
    events: Events,
    body: Vec<u8>,
    overflow: bool,
    calls: HashMap<(u64, u64), Call>,
    kept: usize,
}

impl<'a> Recorder<'a> {
    /// A recorder for `scope`'s calls, of a stream of server-sent events
    /// (`sse`) or a JSON body.
    pub(crate) fn new(signatures: &'a Signatures, scope: &'a str, sse: bool) -> Recorder<'a> {
        Recorder {
            signatures,
            scope,
            sse,
            events: Events::default(),
            body: Vec::new(),
            overflow: false,
            calls: HashMap::new(),
            kept: 0,
        }
    }

    /// Bytes of the answer, in order.
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        if !self.sse {
            if self.overflow || self.body.len() + bytes.len() > MAX_BODY {
                self.overflow = true;
                self.body = Vec::new();
            } else {
                self.body.extend_from_slice(bytes);
            }
            return;
        }
        for data in self.events.feed(bytes) {
            self.chunk(&data);
        }
    }

    /// One chunk of a stream: a call's id and signature may come in one
    /// delta or in several.
    fn chunk(&mut self, data: &str) {
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            return;
        };
        let Some(choices) = chunk.get("choices").and_then(Value::as_array) else {
            return;
        };
        for choice in choices {
            let at = choice["index"].as_u64().unwrap_or(0);
            let Some(deltas) = choice["delta"]["tool_calls"].as_array() else {
                continue;
            };
            for (n, delta) in deltas.iter().enumerate() {
                let index = delta["index"].as_u64().unwrap_or(n as u64);
                let call = self.calls.entry((at, index)).or_default();
                if let Some(id) = delta["id"].as_str().filter(|id| !id.is_empty()) {
                    call.id = Some(id.to_owned());
                }
                if let Some(found) = signature(delta) {
                    call.signature = Some(found.to_owned());
                }
                if let (false, Some(id), Some(found)) = (call.kept, &call.id, &call.signature) {
                    self.signatures.put(self.scope, id, found);
                    call.kept = true;
                    self.kept += 1;
                }
            }
        }
    }

    /// The answer ended: how many signatures it gave that were kept.
    pub(crate) fn finish(mut self) -> usize {
        if self.sse {
            for data in self.events.finish() {
                self.chunk(&data);
            }
            return self.kept;
        }
        if self.overflow {
            return self.kept;
        }
        let Ok(answer) = serde_json::from_slice::<Value>(&self.body) else {
            return self.kept;
        };
        let choices = answer.get("choices").and_then(Value::as_array);
        for choice in choices.into_iter().flatten() {
            let calls = choice["message"]["tool_calls"].as_array();
            for call in calls.into_iter().flatten() {
                let id = call["id"].as_str().filter(|id| !id.is_empty());
                if let (Some(id), Some(found)) = (id, signature(call)) {
                    self.signatures.put(self.scope, id, found);
                    self.kept += 1;
                }
            }
        }
        self.kept
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::{BufReader, Read as _, Write as _};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use branchyard_testkit::{MockHttp, Response as Answer};
    use branchyard_wire as wire;
    use serde_json::json;

    use super::super::gateway::{TurnGateway, TurnState};
    use super::super::{Compat, Config, Gateway, KeySource, Signer};
    use crate::connectors::Actor;
    use crate::Yard;

    const KEY: &str = "sk-real-gemini-key";
    const PERSON: &str = "local:tester";
    const SCOPE: &str = "local:someone";

    #[test]
    fn the_openai_path_loses_its_v1_and_nothing_else() {
        assert_eq!(path("/v1/chat/completions"), "/chat/completions");
        assert_eq!(path("/v1/models?x=1"), "/models?x=1");
        assert_eq!(path("/v1"), "");
        assert_eq!(path("/v1?x=1"), "?x=1");
        assert_eq!(path("/v1beta/models"), "/v1beta/models");
        assert_eq!(path("/v2/chat/completions"), "/v2/chat/completions");
        assert!(is_chat("/v1/chat/completions?beta=1"));
        assert!(!is_chat("/v1/responses"));
        assert!(is_host("generativelanguage.googleapis.com"));
        assert!(is_host("GenerativeLanguage.googleapis.com"));
        assert!(!is_host("aiplatform.googleapis.com"));
    }

    #[test]
    fn only_the_listed_fields_are_stripped() {
        let kept = json!({"model": "gemini-3.5-flash", "messages": [],
            "stream": true, "stream_options": {"include_usage": true},
            "max_completion_tokens": 64, "parallel_tool_calls": false, "logprobs": true,
            "reasoning_effort": "low", "extra_body": {"google": {}}});
        let mut request = kept.clone();
        for name in UNSUPPORTED {
            request[*name] = json!(false);
        }
        let prepared = prepare(&mut request, &Signatures::default(), SCOPE);
        assert_eq!(prepared.stripped, UNSUPPORTED);
        assert_eq!((prepared.recorded, prepared.placeholders), (0, 0));
        assert_eq!(request, kept);
        assert_eq!(
            prepared.to_string(),
            "stripped store, metadata, prediction, prompt_cache_key, safety_identifier, user"
        );
        // Nothing to strip and no calls: nothing changed.
        let mut plain = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});
        let before = plain.clone();
        let prepared = prepare(&mut plain, &Signatures::default(), SCOPE);
        assert!(!prepared.changed());
        assert_eq!(plain, before);
        assert_eq!(prepared.to_string(), "nothing changed");
    }

    fn call(id: &str) -> Value {
        json!({"id": id, "type": "function", "function": {"name": "bash", "arguments": "{}"}})
    }

    fn signed(id: &str, signature: &str) -> Value {
        let mut call = call(id);
        call["extra_content"] = json!({"google": {"thought_signature": signature}});
        call
    }

    #[test]
    fn replayed_calls_get_their_recorded_signature_else_the_first_of_a_step_the_placeholder() {
        let signatures = Signatures::default();
        signatures.put(SCOPE, "fc-1", "SIG-1");
        signatures.put(SCOPE, "fc-3", "SIG-3");
        // Another person's call with the same id is not theirs to replay.
        signatures.put("local:other", "fc-9", "SIG-OTHER");
        let mut request = json!({"model": "gemini-3.5-flash", "messages": [
            {"role": "user", "content": "go"},
            // A step Gemini made: its first call signed, recorded here.
            {"role": "assistant", "content": null, "tool_calls": [call("fc-1"), call("fc-2")]},
            {"role": "tool", "tool_call_id": "fc-1", "content": "a"},
            {"role": "tool", "tool_call_id": "fc-2", "content": "b"},
            // A step from elsewhere: nothing recorded, so the placeholder on
            // its first call only, as Gemini signs only the first.
            {"role": "assistant", "content": null, "tool_calls": [call("x-1"), call("x-2")]},
            // A call that kept its signature keeps it.
            {"role": "assistant", "content": null, "tool_calls": [signed("fc-3", "KEPT")]},
            // Another person's id, and a call with an `extra_content` of the
            // wrong shape.
            {"role": "assistant", "content": null, "tool_calls": [call("fc-9")]},
            {"role": "assistant", "content": null,
                "tool_calls": [{"id": "fc-3", "type": "function", "extra_content": "?",
                    "function": {"name": "bash", "arguments": "{}"}}]},
        ]});
        let prepared = prepare(&mut request, &signatures, SCOPE);
        assert_eq!((prepared.recorded, prepared.placeholders), (2, 2));
        assert!(prepared.stripped.is_empty());
        let messages = &request["messages"];
        let signature_at = |m: usize, c: usize| {
            messages[m]["tool_calls"][c]["extra_content"]["google"]["thought_signature"].clone()
        };
        assert_eq!(signature_at(1, 0), "SIG-1");
        assert_eq!(messages[1]["tool_calls"][1].get("extra_content"), None);
        assert_eq!(signature_at(4, 0), PLACEHOLDER);
        assert_eq!(messages[4]["tool_calls"][1].get("extra_content"), None);
        assert_eq!(signature_at(5, 0), "KEPT");
        assert_eq!(signature_at(6, 0), PLACEHOLDER);
        assert_eq!(signature_at(7, 0), "SIG-3");
        // The rest of a call is as it was.
        assert_eq!(messages[1]["tool_calls"][0]["function"]["name"], "bash");
        assert_eq!(
            prepared.to_string(),
            "thought signatures attached: 2 recorded, 2 placeholder"
        );
    }

    #[test]
    fn the_log_names_fields_and_counts_never_a_signature() {
        let signatures = Signatures::default();
        signatures.put(SCOPE, "fc-1", "SECRET-SIGNATURE-BYTES");
        let mut request = json!({"store": false, "messages": [
            {"role": "assistant", "tool_calls": [call("fc-1")]},
            {"role": "assistant", "tool_calls": [call("x-1")]}]});
        let prepared = prepare(&mut request, &signatures, SCOPE);
        let ((), events) = branchyard_support::testing::capture(|| {
            log("gemini", "gemini-3.5-flash", &prepared);
            log("gemini", "gemini-3.5-flash", &Prepared::default());
        });
        assert_eq!(events.len(), 1, "{events:?}");
        let event = &events[0];
        assert_eq!(event.level, branchyard_support::testing::Level::DEBUG);
        for needle in [
            "stripped store",
            "1 recorded, 1 placeholder",
            "backend=\"gemini\"",
            "model=\"gemini-3.5-flash\"",
        ] {
            assert!(event.text.contains(needle), "{needle}: {}", event.text);
        }
        assert!(!event.text.contains("SECRET"), "{}", event.text);
        assert!(!event.text.contains(PLACEHOLDER), "{}", event.text);
    }

    #[test]
    fn signatures_are_bounded_and_the_least_recently_used_goes_first() {
        let signatures = Signatures::bounded(2, 1024);
        signatures.put(SCOPE, "a", "A");
        signatures.put(SCOPE, "b", "B");
        // Reading `a` makes `b` the least recently used.
        assert_eq!(signatures.get(SCOPE, "a").as_deref(), Some("A"));
        signatures.put(SCOPE, "c", "C");
        assert_eq!(signatures.len(), 2);
        assert_eq!(signatures.get(SCOPE, "b"), None);
        assert_eq!(signatures.get(SCOPE, "a").as_deref(), Some("A"));
        assert_eq!(signatures.get(SCOPE, "c").as_deref(), Some("C"));
        // Kept again, a call's signature is replaced, not added.
        signatures.put(SCOPE, "c", "C2");
        assert_eq!(signatures.len(), 2);
        assert_eq!(signatures.get(SCOPE, "c").as_deref(), Some("C2"));
        // The same id is another call for another person.
        assert_eq!(signatures.get("local:other", "c"), None);
        // Bytes bound it too: one large signature pushes the rest out, and
        // one larger than the bound is not kept, nor what it replaced.
        let bytes = Signatures::bounded(100, 64);
        bytes.put(SCOPE, "a", "A");
        bytes.put(SCOPE, "b", &"B".repeat(40));
        assert_eq!(bytes.len(), 1);
        assert_eq!(bytes.get(SCOPE, "a"), None);
        bytes.put(SCOPE, "b", &"B".repeat(80));
        assert_eq!(bytes.len(), 0);
        bytes.put(SCOPE, "c", "C");
        assert_eq!(bytes.get(SCOPE, "c").as_deref(), Some("C"));
        let full = Signatures::default();
        for n in 0..MAX_ENTRIES + 10 {
            full.put(SCOPE, &format!("call-{n}"), "S");
        }
        assert_eq!(full.len(), MAX_ENTRIES);
        assert_eq!(full.get(SCOPE, "call-9"), None);
        assert!(full.get(SCOPE, "call-10").is_some());
    }

    #[test]
    fn a_whole_answers_signatures_are_kept_when_it_ends() {
        let answer = json!({"choices": [
            {"index": 0, "message": {"role": "assistant", "tool_calls": [
                signed("fc-1", "SIG-1"), call("fc-2"), signed("", "NO-ID")]}},
            {"index": 1, "message": {"role": "assistant", "tool_calls": [signed("fc-3", "SIG-3")]}},
        ]})
        .to_string();
        let signatures = Signatures::default();
        let mut recorder = Recorder::new(&signatures, SCOPE, false);
        for piece in answer.as_bytes().chunks(7) {
            recorder.feed(piece);
        }
        assert_eq!(signatures.len(), 0);
        assert_eq!(recorder.finish(), 2);
        assert_eq!(signatures.get(SCOPE, "fc-1").as_deref(), Some("SIG-1"));
        assert_eq!(signatures.get(SCOPE, "fc-2"), None);
        assert_eq!(signatures.get(SCOPE, "fc-3").as_deref(), Some("SIG-3"));
        // An answer that is not JSON keeps nothing.
        let mut broken = Recorder::new(&signatures, SCOPE, false);
        broken.feed(b"{\"choices\": [");
        assert_eq!(broken.finish(), 0);
    }

    fn event(chunk: &Value) -> String {
        format!("data: {chunk}\r\n\r\n")
    }

    #[test]
    fn a_streams_signatures_are_kept_as_each_event_completes_whatever_the_chunking() {
        let delta =
            |calls: Value| json!({"choices": [{"index": 0, "delta": {"tool_calls": calls}}]});
        let stream = [
            event(&json!({"choices": [{"index": 0, "delta": {"role": "assistant"}}]})),
            // The id and signature in one delta, as Gemini sends them.
            event(&delta(
                json!([{"index": 0, "id": "fc-1", "type": "function",
                "function": {"name": "bash", "arguments": "{}"},
                "extra_content": {"google": {"thought_signature": "SIG-1"}}}]),
            )),
            // Or the id first and the signature in a later delta.
            event(&delta(
                json!([{"index": 1, "id": "fc-2", "type": "function",
                "function": {"name": "bash", "arguments": ""}}]),
            )),
            event(&delta(json!([{"index": 1, "function": {"arguments": "{}"},
                "extra_content": {"google": {"thought_signature": "SIG-2"}}}]))),
            // A second choice's call of the same index is another call.
            event(
                &json!({"choices": [{"index": 1, "delta": {"tool_calls": [{"index": 0,
                "id": "fc-3", "extra_content": {"google": {"thought_signature": "SIG-3"}}}]}}]}),
            ),
            event(&json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]})),
            "data: [DONE]\n\n".to_owned(),
        ]
        .concat();
        for size in [1, 5, stream.len()] {
            let signatures = Signatures::default();
            let mut recorder = Recorder::new(&signatures, SCOPE, true);
            for piece in stream.as_bytes().chunks(size) {
                recorder.feed(piece);
            }
            // Kept before the stream ends: the harness may act on `[DONE]`.
            assert_eq!(signatures.len(), 3, "chunks of {size}");
            assert_eq!(recorder.finish(), 3);
            for (id, signature) in [("fc-1", "SIG-1"), ("fc-2", "SIG-2"), ("fc-3", "SIG-3")] {
                assert_eq!(signatures.get(SCOPE, id).as_deref(), Some(signature));
            }
        }
        // A last event without its blank line still counts.
        let signatures = Signatures::default();
        let mut recorder = Recorder::new(&signatures, SCOPE, true);
        recorder.feed(
            format!(
                "data: {}",
                json!({"choices": [{"delta": {"tool_calls": [{"id": "fc-9",
                    "extra_content": {"google": {"thought_signature": "SIG-9"}}}]}}]})
            )
            .as_bytes(),
        );
        assert_eq!(recorder.finish(), 1);
        assert_eq!(signatures.get(SCOPE, "fc-9").as_deref(), Some("SIG-9"));
    }

    fn signer(dir: &std::path::Path) -> Signer {
        Signer {
            issuer: "branchyard:test".into(),
            key_file: dir.join("key"),
            jwks_file: None,
            subject: PERSON.into(),
            tenant: "local".into(),
        }
    }

    fn compat_of(backend: Value) -> Result<Compat, String> {
        let config: Config = serde_json::from_value(json!({"backends": {"b": backend}}))
            .map_err(|e| e.to_string())?;
        let dir = std::env::temp_dir();
        Gateway::new(&config, signer(&dir), |name| KeySource::Env(name.into()))
            .map(|g| g.backends[0].compat)
    }

    #[test]
    fn the_layer_is_on_for_geminis_host_and_where_it_is_set() {
        let gemini = "https://generativelanguage.googleapis.com/v1beta/openai";
        assert_eq!(
            compat_of(json!({"api": "openai", "url": gemini})),
            Ok(Compat::Gemini)
        );
        assert_eq!(
            compat_of(json!({"api": "openai", "url": format!("{gemini}/")})),
            Ok(Compat::Gemini)
        );
        assert_eq!(
            compat_of(json!({"api": "openai", "url": gemini, "compat": "none"})),
            Ok(Compat::Off)
        );
        assert_eq!(compat_of(json!({"api": "openai"})), Ok(Compat::Off));
        assert_eq!(
            compat_of(
                json!({"api": "openai", "url": "https://proxy.example/gemini",
                "compat": "gemini"})
            ),
            Ok(Compat::Gemini)
        );
        // Gemini's own API, passed through, is not its OpenAI endpoint.
        assert_eq!(
            compat_of(
                json!({"api": "generic", "url": "https://generativelanguage.googleapis.com"})
            ),
            Ok(Compat::Off)
        );
        // Z.ai's OpenAI-compatible endpoints name their version in the URL:
        // the path's /v1 is dropped and nothing else is adapted.
        for url in [
            "https://api.z.ai/api/coding/paas/v4",
            "https://api.z.ai/api/paas/v4",
            "https://api.z.ai/api/v1",
        ] {
            assert_eq!(
                compat_of(json!({"api": "openai", "url": url})),
                Ok(Compat::Versioned),
                "{url}"
            );
        }
        assert_eq!(
            compat_of(
                json!({"api": "openai", "url": "https://api.z.ai/api/paas/v4", "compat": "none"})
            ),
            Ok(Compat::Off)
        );
        assert_eq!(
            compat_of(
                json!({"api": "openai", "url": "https://proxy.example/v4", "compat": "versioned"})
            ),
            Ok(Compat::Versioned)
        );
        // Its Anthropic endpoint is an anthropic backend, with no layer.
        assert_eq!(
            compat_of(json!({"api": "anthropic", "url": "https://api.z.ai/api/anthropic"})),
            Ok(Compat::Off)
        );
        let refused = compat_of(json!({"api": "anthropic", "compat": "versioned"})).unwrap_err();
        assert!(refused.contains("openai backend"), "{refused}");
        let refused = compat_of(json!({"api": "anthropic", "compat": "gemini"})).unwrap_err();
        assert!(
            refused.contains("models.backends.b.compat") && refused.contains("openai backend"),
            "{refused}"
        );
        let config: Result<Config, _> = serde_json::from_value(
            json!({"backends": {"b": {"api": "openai", "compat": "vertex"}}}),
        );
        assert!(config.is_err());
    }

    /// A turn's gateway in front of mock upstreams, with a token that may
    /// call any model, acting for [`PERSON`].
    struct Turn {
        _dir: tempfile::TempDir,
        gateway: Arc<Gateway>,
        address: SocketAddr,
        token: String,
        _running: TurnGateway,
    }

    fn turn(config: Value) -> Turn {
        let dir = tempfile::Builder::new()
            .prefix("by-gemini-")
            .tempdir()
            .unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::write(dir.path().join("gemini-key"), KEY).unwrap();
        let yard = Yard::open(dir.path()).unwrap();
        let config: Config = serde_json::from_value(config).unwrap();
        let keys_in = dir.path().to_owned();
        let gateway = Gateway::new(&config, signer(dir.path()), move |name| {
            KeySource::File(keys_in.join(name))
        })
        .unwrap();
        let gateway = Arc::new(gateway);
        let keys = gateway.signer.keys().unwrap();
        let now = branchyard_support::time::now_ms() / 1000;
        let scopes = crate::access::TokenScopes {
            models: Some(vec!["*".into()]),
            network: None,
            delegation: None,
        };
        let claims = crate::connectors::turn_claims(crate::connectors::TurnToken {
            issuer: &gateway.signer.issuer,
            audience: "http://gateway.test",
            actor: Actor {
                subject: PERSON.into(),
                tenant: "local".into(),
            },
            by_branch: "gem".into(),
            turn: 1,
            window: (now, now + 600),
            grants: Vec::new(),
            scopes: &scopes,
        })
        .unwrap();
        let token = keys.sign(&claims).unwrap();
        let state = TurnState {
            yard,
            gateway: gateway.clone(),
            jwks: keys.jwks().unwrap(),
            token: token.clone(),
            branch: "gem".into(),
            by_branch: "gem".into(),
            turn: 1,
            subject: PERSON.into(),
            max_usd: None,
            cost_before: 0.0,
            held: 0.0,
            metered: Mutex::new(0.0),
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let running = TurnGateway::start(listener, Arc::new(state)).unwrap();
        Turn {
            _dir: dir,
            gateway,
            address,
            token,
            _running: running,
        }
    }

    impl Turn {
        /// POST `body` to the gateway at `path` as a harness does: the
        /// status and the answer's body.
        fn post(&self, path: &str, body: &Value) -> (u16, String) {
            let mut stream = TcpStream::connect(self.address).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(30)))
                .unwrap();
            let bytes = body.to_string();
            let bearer = format!("Bearer {}", self.token);
            let head = wire::request_head(
                "POST",
                path,
                [
                    ("Host", "gateway.test"),
                    ("Authorization", bearer.as_str()),
                    ("Content-Type", "application/json"),
                ],
                Some(bytes.len() as u64),
            )
            .unwrap();
            stream.write_all(&head).unwrap();
            stream.write_all(bytes.as_bytes()).unwrap();
            let mut reader = BufReader::new(stream);
            let head = wire::read_response_head(&mut reader, 64 * 1024).unwrap();
            let framing = wire::response_framing(head.status, &head.headers, false).unwrap();
            let mut answer = String::new();
            wire::Body::new(reader, framing)
                .read_to_string(&mut answer)
                .unwrap();
            (head.status, answer)
        }
    }

    /// A mock of Gemini's OpenAI-compatible endpoint. Strict, it refuses
    /// what the real one was measured to refuse: `store`, and a step whose
    /// first tool call has no signature. Each answer calls `bash` once as
    /// `fc-N`, signed `SIG-N`, for the N-th request; streamed when asked,
    /// with the signature in the call's delta, as Gemini sends it.
    fn gemini(strict: bool) -> MockHttp {
        let n = AtomicUsize::new(0);
        MockHttp::start(move |request| {
            let n = n.fetch_add(1, Ordering::SeqCst) + 1;
            let body = request.json();
            let refuse = |message: &str| {
                Answer::json(
                    400,
                    &json!({"error": {"code": 400, "message": message,
                        "status": "INVALID_ARGUMENT"}}),
                )
            };
            if strict && body.get("store").is_some() {
                return refuse(
                    "Invalid JSON payload received. Unknown name \"store\": Cannot find field.",
                );
            }
            let messages = body["messages"].as_array().cloned().unwrap_or_default();
            for (position, message) in messages.iter().enumerate() {
                let first = &message["tool_calls"][0];
                if strict && first.is_object() && signature(first).is_none() {
                    return refuse(&format!(
                        "Function call is missing a thought_signature in functionCall parts. \
                         function call default_api:bash, position {}",
                        position + 1
                    ));
                }
            }
            let call = json!({"id": format!("fc-{n}"), "type": "function",
                "function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"},
                "extra_content": {"google": {"thought_signature": format!("SIG-{n}")}}});
            let usage = json!({"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12});
            let model = body["model"].clone();
            if body["stream"] == json!(true) {
                let mut delta = call.clone();
                delta["index"] = json!(0);
                let stream = [
                    event(&json!({"model": model, "choices": [{"index": 0,
                        "delta": {"role": "assistant", "tool_calls": [delta]}}]})),
                    event(
                        &json!({"model": model, "choices": [{"index": 0, "delta": {},
                        "finish_reason": "tool_calls"}]}),
                    ),
                    event(&json!({"model": model, "choices": [], "usage": usage})),
                    "data: [DONE]\r\n\r\n".to_owned(),
                ]
                .concat();
                return Answer::new(200, stream).header("Content-Type", "text/event-stream");
            }
            Answer::json(
                200,
                &json!({"id": format!("resp-{n}"), "object": "chat.completion", "model": model,
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": null,
                        "tool_calls": [call]}, "finish_reason": "tool_calls"}],
                    "usage": usage}),
            )
        })
    }

    fn on_gemini(mock: &MockHttp) -> Value {
        json!({"api": "openai", "url": format!("{}/v1beta/openai", mock.url()),
            "key": "gemini-key", "compat": "gemini"})
    }

    fn chat(messages: Value, stream: bool) -> Value {
        json!({"model": "gemini-3.5-flash", "messages": messages, "store": false,
            "max_completion_tokens": 64, "stream": stream,
            "tools": [{"type": "function", "function": {"name": "bash",
                "parameters": {"type": "object"}}}]})
    }

    fn replay(id: &str) -> Value {
        json!([
            {"role": "user", "content": "list the files"},
            {"role": "assistant", "content": null, "tool_calls": [call(id)]},
            {"role": "tool", "tool_call_id": id, "content": "a.txt"},
        ])
    }

    fn sent_signature(request: &branchyard_testkit::Request, message: usize) -> Value {
        request.json()["messages"][message]["tool_calls"][0]["extra_content"]["google"]
            ["thought_signature"]
            .clone()
    }

    #[test]
    fn an_openai_harness_reaches_gemini_stripped_and_its_calls_signed() {
        let mock = gemini(true);
        let turn = turn(json!({"backends": {"gemini": on_gemini(&mock)}}));
        let first = json!([{"role": "user", "content": "list the files"}]);
        let (status, answer) = turn.post("/openai/v1/chat/completions", &chat(first, false));
        assert_eq!(status, 200, "{answer}");
        // The harness gets Gemini's answer as it was, signature and all.
        let answer: Value = serde_json::from_str(&answer).unwrap();
        let sent = &answer["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(sent["id"], "fc-1");
        assert_eq!(signature(sent), Some("SIG-1"));
        // The call the harness replays without the signature gets it back.
        let (status, answer) =
            turn.post("/openai/v1/chat/completions", &chat(replay("fc-1"), false));
        assert_eq!(status, 200, "{answer}");
        // A call from a conversation begun elsewhere gets the placeholder.
        let (status, answer) = turn.post(
            "/openai/v1/chat/completions",
            &chat(replay("call_elsewhere"), false),
        );
        assert_eq!(status, 200, "{answer}");
        let seen = mock.requests();
        assert_eq!(seen.len(), 3);
        for request in &seen {
            // Gemini's base URL stands for OpenAI's `/v1`.
            assert_eq!(request.path, "/v1beta/openai/chat/completions");
            let bearer = format!("Bearer {KEY}");
            assert_eq!(request.header("authorization"), Some(bearer.as_str()));
            let body = request.json();
            assert_eq!(body.get("store"), None, "{body}");
            assert_eq!(body["max_completion_tokens"], 64);
            assert_eq!(body["tools"][0]["function"]["name"], "bash");
        }
        assert_eq!(sent_signature(&seen[1], 1), "SIG-1");
        assert_eq!(sent_signature(&seen[2], 1), PLACEHOLDER);
        // What was kept: the signatures of the three answers.
        for n in 1..=3 {
            assert_eq!(
                turn.gateway.signatures.get(PERSON, &format!("fc-{n}")),
                Some(format!("SIG-{n}"))
            );
        }
        mock.finish();
    }

    #[test]
    fn a_streamed_answers_signatures_are_kept_from_its_deltas() {
        let mock = gemini(true);
        let turn = turn(json!({"backends": {"gemini": on_gemini(&mock)}}));
        let first = json!([{"role": "user", "content": "list the files"}]);
        let (status, answer) = turn.post("/openai/v1/chat/completions", &chat(first, true));
        assert_eq!(status, 200, "{answer}");
        assert!(answer.contains("\"id\":\"fc-1\""), "{answer}");
        assert!(answer.contains("data: [DONE]"), "{answer}");
        let (status, answer) =
            turn.post("/openai/v1/chat/completions", &chat(replay("fc-1"), true));
        assert_eq!(status, 200, "{answer}");
        let seen = mock.requests();
        assert_eq!(seen.len(), 2);
        // The stream was asked for its usage, as every chat stream is.
        assert_eq!(seen[0].json()["stream_options"]["include_usage"], true);
        assert_eq!(seen[1].json().get("store"), None);
        assert_eq!(sent_signature(&seen[1], 1), "SIG-1");
        mock.finish();
    }

    #[test]
    fn a_claude_request_translated_for_gemini_keeps_its_signatures_through_tool_use_ids() {
        let mock = gemini(true);
        let turn = turn(json!({
            "backends": {"gemini": on_gemini(&mock)},
            "routes": [{"model": "claude-*", "backends": ["gemini"],
                "backend_model": "gemini-3.5-flash"}]
        }));
        let tools = json!([{"name": "bash", "description": "run",
            "input_schema": {"type": "object"}}]);
        let claude = |messages: Value, stream: bool| {
            json!({"model": "claude-sonnet-4-6", "max_tokens": 64, "stream": stream,
                "metadata": {"user_id": "user_abc"}, "tools": tools, "messages": messages})
        };
        let tool_use = |id: &str| {
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": id, "name": "bash", "input": {"command": "ls"}}]})
        };
        let tool_result = |id: &str| {
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": id, "content": "a.txt"}]})
        };
        let ask = json!({"role": "user", "content": "list the files"});
        // Streamed: Gemini's call reaches Claude Code as a tool_use of the
        // same id.
        let (status, answer) = turn.post("/anthropic/v1/messages", &claude(json!([ask]), true));
        assert_eq!(status, 200, "{answer}");
        assert!(answer.contains("\"type\":\"tool_use\""), "{answer}");
        assert!(answer.contains("\"id\":\"fc-1\""), "{answer}");
        // Replayed as Claude Code does, without a signature, it is put
        // back; this answer is whole.
        let replayed = json!([ask, tool_use("fc-1"), tool_result("fc-1")]);
        let (status, answer) = turn.post("/anthropic/v1/messages", &claude(replayed, false));
        assert_eq!(status, 200, "{answer}");
        let message: Value = serde_json::from_str(&answer).unwrap();
        assert_eq!(message["content"][0]["type"], "tool_use");
        assert_eq!(message["content"][0]["id"], "fc-2");
        // A step from elsewhere, and one from the whole answer above.
        let both = json!([
            ask,
            tool_use("toolu_elsewhere"),
            tool_result("toolu_elsewhere"),
            tool_use("fc-2"),
            tool_result("fc-2")
        ]);
        let (status, answer) = turn.post("/anthropic/v1/messages", &claude(both, false));
        assert_eq!(status, 200, "{answer}");
        let seen = mock.requests();
        assert_eq!(seen.len(), 3);
        for request in &seen {
            assert_eq!(request.path, "/v1beta/openai/chat/completions");
            let body = request.json();
            assert_eq!(body["model"], "gemini-3.5-flash");
            // Claude Code's user id became `user`, which Gemini is not sent.
            assert_eq!(body.get("user"), None, "{body}");
        }
        assert_eq!(sent_signature(&seen[1], 1), "SIG-1");
        assert_eq!(sent_signature(&seen[2], 1), PLACEHOLDER);
        assert_eq!(sent_signature(&seen[2], 3), "SIG-2");
        mock.finish();
    }

    #[test]
    fn a_backend_without_the_layer_is_sent_what_the_harness_sent() {
        let mock = gemini(false);
        let turn = turn(
            json!({"backends": {"openai": {"api": "openai", "url": mock.url(),
            "key": "gemini-key"}}}),
        );
        assert_eq!(turn.gateway.backends[0].compat, Compat::Off);
        let request = chat(replay("fc-1"), false);
        let (status, answer) = turn.post("/openai/v1/chat/completions", &request);
        assert_eq!(status, 200, "{answer}");
        let seen = mock.requests();
        assert_eq!(seen.len(), 1);
        // Its path, `store` and the unsigned call are as the harness sent
        // them, byte for byte.
        assert_eq!(seen[0].path, "/v1/chat/completions");
        assert_eq!(seen[0].body, request.to_string().into_bytes());
        // And a signature in its answer is not kept.
        assert_eq!(turn.gateway.signatures.len(), 0);
        mock.finish();
    }
}
