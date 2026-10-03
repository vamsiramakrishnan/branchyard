//! The gateway's wire as the ledger needs it: MCP Streamable HTTP
//! (JSON-RPC over POST, answered with JSON or a server-sent event stream),
//! the effect metadata the gateway returns (`_meta.effect` or the
//! `X-Anvil-Effect` header), and what `tools/list` declares about each
//! operation before it is called. See `docs/effects.md#the-wire`.

use std::io::Read;

use serde_json::{json, Value};

use super::{EffectClass, Lookup, Undo, UndoKind};
use crate::models::upstream::{self, Failure, Target};

/// The MCP revision Branchyard offers.
pub const PROTOCOL_VERSION: &str = "2025-06-18";
/// The header carrying effect metadata on a REST answer.
pub const EFFECT_HEADER: &str = "X-Anvil-Effect";
/// Largest gateway answer read.
pub const MAX_ANSWER: usize = 16 * 1024 * 1024;

/// What the gateway said about one call's effect.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EffectMeta {
    pub class: Option<EffectClass>,
    /// `Some(None)` when the gateway said there is no undo.
    pub undo: Option<Option<Undo>>,
    pub idempotency_key: Option<String>,
    pub summary: Option<String>,
    /// A staged call's draft handle.
    pub draft: Option<String>,
    /// A lookup's verdict: whether the call it asks about happened.
    pub found: Option<bool>,
    /// A lookup's description of the call it found (`lookup.class`,
    /// `lookup.undo`), not of the lookup itself.
    pub found_effect: Option<Box<EffectMeta>>,
}

impl EffectMeta {
    /// From a `tools/call` result's `_meta.effect`, else the header.
    pub fn read(result: Option<&Value>, header: Option<&str>) -> Option<EffectMeta> {
        let from_result = result
            .and_then(|r| r.get("_meta"))
            .and_then(|m| m.get("effect"))
            .filter(|e| e.is_object())
            .cloned();
        let value = match from_result {
            Some(value) => value,
            None => serde_json::from_str::<Value>(header?).ok()?,
        };
        value.is_object().then(|| EffectMeta::parse(&value))
    }

    fn parse(value: &Value) -> EffectMeta {
        let class = value
            .get("class")
            .and_then(Value::as_str)
            .and_then(|c| EffectClass::parse(c).ok());
        let deadline = value.get("deadline_ms").and_then(Value::as_u64);
        let undo = match value.get("undo") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(undo) => Some(undo.get("operation").and_then(Value::as_str).map(|op| {
                let kind = match undo.get("kind").and_then(Value::as_str) {
                    Some("compensate") => UndoKind::Compensate,
                    Some("inverse") => UndoKind::Inverse,
                    _ if class == Some(EffectClass::Compensable) => UndoKind::Compensate,
                    _ => UndoKind::Inverse,
                };
                Undo {
                    operation: op.to_owned(),
                    arguments: undo.get("arguments").cloned().unwrap_or(json!({})),
                    kind,
                    deadline_ms: undo.get("deadline_ms").and_then(Value::as_u64).or(deadline),
                    summary: undo
                        .get("summary")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                }
            })),
        };
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
        EffectMeta {
            class,
            undo,
            idempotency_key: text("idempotency_key"),
            summary: text("summary"),
            draft: value
                .get("staged")
                .and_then(|s| s.get("handle"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            found: value
                .get("lookup")
                .and_then(|l| l.get("found"))
                .and_then(Value::as_bool),
            found_effect: value
                .get("lookup")
                .filter(|l| l.get("class").is_some() || l.get("undo").is_some())
                .map(|l| Box::new(EffectMeta::parse(l))),
        }
    }
}

/// What `tools/list` declares about a tool before it is called: Anvil's
/// MCP annotations and `_meta.effect`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolDecl {
    /// The AIR operation id, when declared.
    pub operation: Option<String>,
    pub class: Option<EffectClass>,
    pub read_only: bool,
    pub deletion: bool,
    /// Whether the operation has a draft form (`stage: true`).
    pub draft: bool,
    pub lookup: Option<Lookup>,
}

impl ToolDecl {
    pub fn read(tool: &Value) -> ToolDecl {
        let annotations = tool.get("annotations");
        let hint = |name: &str| {
            annotations
                .and_then(|a| a.get(name))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        };
        let effect = tool.get("_meta").and_then(|m| m.get("effect"));
        let field = |name: &str| effect.and_then(|e| e.get(name));
        ToolDecl {
            operation: field("operation")
                .and_then(Value::as_str)
                .map(str::to_owned),
            class: field("class")
                .and_then(Value::as_str)
                .and_then(|c| EffectClass::parse(c).ok()),
            read_only: hint("readOnlyHint"),
            deletion: field("deletion").and_then(Value::as_bool).unwrap_or(false),
            draft: match field("draft") {
                Some(Value::Bool(b)) => *b,
                Some(Value::Object(_)) => true,
                _ => false,
            },
            lookup: field("lookup").and_then(|l| {
                Some(Lookup {
                    operation: l.get("operation")?.as_str()?.to_owned(),
                    arguments: l.get("arguments").cloned().unwrap_or(json!({})),
                })
            }),
        }
    }

    /// The class before the call: declared, else a read when annotated
    /// read-only, else the worst case.
    pub fn class(&self) -> EffectClass {
        match (self.class, self.read_only) {
            (Some(class), _) => class,
            (None, true) => EffectClass::Read,
            (None, false) => EffectClass::Irreversible,
        }
    }
}

/// The JSON-RPC message answering `id` in a response body: a JSON object
/// (or array), or the `data:` lines of a server-sent event stream.
pub fn rpc_answer(content_type: Option<&str>, body: &[u8], id: &Value) -> Option<Value> {
    let sse = content_type.is_some_and(|t| t.to_ascii_lowercase().starts_with("text/event-stream"));
    let matches = |v: &Value| v.get("id") == Some(id) && v.get("method").is_none();
    let pick = |v: Value| -> Option<Value> {
        match v {
            Value::Array(items) => items.into_iter().find(|i| matches(i)),
            v if matches(&v) => Some(v),
            _ => None,
        }
    };
    if !sse {
        return pick(serde_json::from_slice(body).ok()?);
    }
    let text = String::from_utf8_lossy(body);
    let mut data = String::new();
    let mut found = None;
    for line in text.lines().chain(std::iter::once("")) {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        } else if line.is_empty() && !data.is_empty() {
            if let Some(v) = serde_json::from_str::<Value>(&data).ok().and_then(&pick) {
                found = Some(v);
            }
            data.clear();
        }
    }
    found
}

/// Why a call through the gateway has no answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallError {
    /// Nothing was sent: the call did not happen.
    NotSent(String),
    /// It was sent, and the answer was lost: it may have happened.
    Lost(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::NotSent(why) => write!(f, "not sent: {why}"),
            CallError::Lost(why) => write!(f, "the answer was lost: {why}"),
        }
    }
}

/// A `tools/call` answer.
#[derive(Clone, Debug, PartialEq)]
pub struct Called {
    pub status: u16,
    /// Whether the body held a JSON-RPC answer at all.
    pub answered: bool,
    /// The JSON-RPC result, when there is one.
    pub result: Option<Value>,
    /// The JSON-RPC error, or the HTTP failure, when there is no result.
    pub error: Option<String>,
    pub meta: Option<EffectMeta>,
}

impl Called {
    /// Whether the upstream did what was asked: a result that is not a
    /// tool error.
    pub fn ok(&self) -> bool {
        self.result
            .as_ref()
            .is_some_and(|r| r.get("isError") != Some(&Value::Bool(true)))
    }

    /// The upstream's answer in a line: the tool error's text, the
    /// JSON-RPC error, or the status.
    pub fn answer(&self) -> String {
        if let Some(error) = &self.error {
            return error.clone();
        }
        let text = self
            .result
            .as_ref()
            .and_then(|r| r.get("content"))
            .and_then(Value::as_array)
            .and_then(|items| {
                items
                    .iter()
                    .find_map(|i| i.get("text").and_then(Value::as_str))
            })
            .unwrap_or("");
        let text: String = text.chars().take(300).collect();
        match (self.ok(), text.is_empty()) {
            (true, true) => format!("{} ok", self.status),
            (true, false) => text,
            (false, true) => format!("{} tool error", self.status),
            (false, false) => format!("tool error: {text}"),
        }
    }
}

/// A client of the gateway's `/mcp`, with a token: what the ledger uses
/// for lookups, inverses and promotions, and to read `tools/list`.
#[derive(Clone, Debug)]
pub struct Client {
    pub url: String,
    pub token: String,
}

impl Client {
    pub fn new(url: impl Into<String>, token: impl Into<String>) -> Client {
        Client {
            url: url.into(),
            token: token.into(),
        }
    }

    fn post(
        &self,
        target: &Target,
        session: Option<&str>,
        body: &Value,
        extra: &[(String, String)],
    ) -> Result<upstream::Response, Failure> {
        let mut headers = vec![
            ("Authorization".to_owned(), format!("Bearer {}", self.token)),
            ("Content-Type".to_owned(), "application/json".to_owned()),
            (
                "Accept".to_owned(),
                "application/json, text/event-stream".to_owned(),
            ),
            ("Accept-Encoding".to_owned(), "identity".to_owned()),
            (
                "Mcp-Protocol-Version".to_owned(),
                PROTOCOL_VERSION.to_owned(),
            ),
        ];
        if let Some(session) = session {
            headers.push(("Mcp-Session-Id".to_owned(), session.to_owned()));
        }
        headers.extend(extra.iter().cloned());
        let bytes = serde_json::to_vec(body).unwrap_or_default();
        upstream::send(target, "POST", "", &headers, &bytes)
    }

    /// `initialize` and `notifications/initialized`: the session id, if
    /// the gateway keeps sessions.
    fn open(&self, target: &Target) -> Result<Option<String>, String> {
        let init = json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "branchyard-effects", "version": env!("CARGO_PKG_VERSION")}
            }
        });
        let mut response = self
            .post(target, None, &init, &[])
            .map_err(|e| e.to_string())?;
        let session = response.header("mcp-session-id").map(str::to_owned);
        let mut body = Vec::new();
        let _ = (&mut response.body)
            .take(MAX_ANSWER as u64)
            .read_to_end(&mut body);
        if !(200..300).contains(&response.status) {
            return Err(format!(
                "initialize: {} {}",
                response.status,
                String::from_utf8_lossy(&body)
            ));
        }
        let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        if let Ok(mut response) = self.post(target, session.as_deref(), &note, &[]) {
            let _ = std::io::copy(&mut response.body, &mut std::io::sink());
        }
        Ok(session)
    }

    fn close(&self, target: &Target, session: Option<&str>) {
        if let Some(session) = session {
            let headers = vec![
                ("Authorization".to_owned(), format!("Bearer {}", self.token)),
                ("Mcp-Session-Id".to_owned(), session.to_owned()),
            ];
            if let Ok(mut response) = upstream::send(target, "DELETE", "", &headers, &[]) {
                let _ = std::io::copy(&mut response.body, &mut std::io::sink());
            }
        }
    }

    /// Call `tool` (the wire name, `<connector>__<tool>`) with `arguments`
    /// and `_meta` (the idempotency key and any staging flags), sending
    /// `idempotency` as `Idempotency-Key` too.
    pub fn call(
        &self,
        tool: &str,
        arguments: &Value,
        meta: &Value,
        idempotency: Option<&str>,
    ) -> Result<Called, CallError> {
        let target = Target::parse(&self.url).map_err(CallError::NotSent)?;
        let session = self.open(&target).map_err(CallError::NotSent)?;
        let mut params = json!({"name": tool, "arguments": arguments});
        if meta.as_object().is_some_and(|m| !m.is_empty()) {
            params["_meta"] = meta.clone();
        }
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": params});
        let extra: Vec<(String, String)> = idempotency
            .map(|k| vec![("Idempotency-Key".to_owned(), k.to_owned())])
            .unwrap_or_default();
        let answered = self.post(&target, session.as_deref(), &request, &extra);
        let outcome = match answered {
            Err(Failure::Connect(why)) => Err(CallError::NotSent(why)),
            Err(Failure::Exchange(why)) => Err(CallError::Lost(why)),
            Ok(mut response) => {
                let mut body = Vec::new();
                match (&mut response.body)
                    .take(MAX_ANSWER as u64)
                    .read_to_end(&mut body)
                {
                    Err(e) => Err(CallError::Lost(e.to_string())),
                    Ok(_) => Ok(called(&response, &body, &json!(1))),
                }
            }
        };
        self.close(&target, session.as_deref());
        outcome
    }

    /// Every tool the gateway lists for this token.
    pub fn list_tools(&self) -> Result<Vec<Value>, String> {
        let target = Target::parse(&self.url)?;
        let session = self.open(&target)?;
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        let outcome = loop {
            let mut params = json!({});
            if let Some(cursor) = &cursor {
                params["cursor"] = json!(cursor);
            }
            let request =
                json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": params});
            let mut response = match self.post(&target, session.as_deref(), &request, &[]) {
                Ok(response) => response,
                Err(e) => break Err(e.to_string()),
            };
            let mut body = Vec::new();
            let _ = (&mut response.body)
                .take(MAX_ANSWER as u64)
                .read_to_end(&mut body);
            let content_type = response.header("content-type").map(str::to_owned);
            let Some(answer) = rpc_answer(content_type.as_deref(), &body, &json!(2)) else {
                break Err(format!("tools/list: {} without an answer", response.status));
            };
            let Some(result) = answer.get("result") else {
                break Err(format!("tools/list: {}", answer["error"]));
            };
            if let Some(page) = result.get("tools").and_then(Value::as_array) {
                tools.extend(page.iter().cloned());
            }
            match result.get("nextCursor").and_then(Value::as_str) {
                Some(next) if Some(next) != cursor.as_deref() && tools.len() < 10_000 => {
                    cursor = Some(next.to_owned())
                }
                _ => break Ok(()),
            }
        };
        self.close(&target, session.as_deref());
        outcome.map(|()| tools)
    }
}

/// A `tools/call` answer from its HTTP response.
pub fn called(response: &upstream::Response, body: &[u8], id: &Value) -> Called {
    let answer = rpc_answer(response.header("content-type"), body, id);
    let result = answer.as_ref().and_then(|a| a.get("result")).cloned();
    let error = match (&answer, &result) {
        (_, Some(_)) => None,
        (Some(a), None) => Some(match a.get("error") {
            Some(e) => e
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| e.to_string()),
            None => "an answer with neither a result nor an error".to_owned(),
        }),
        (None, None) => Some(format!(
            "{} {}",
            response.status,
            String::from_utf8_lossy(&body[..body.len().min(300)])
        )),
    };
    let meta = EffectMeta::read(result.as_ref(), response.header(EFFECT_HEADER));
    Called {
        status: response.status,
        answered: answer.is_some(),
        result,
        error,
        meta,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effect_metadata_is_read_from_meta_or_the_header() {
        let result = json!({"content": [], "_meta": {"effect": {
            "class": "reversible",
            "undo": {"operation": "comments_delete", "arguments": {"id": 7}},
            "deadline_ms": 99, "idempotency_key": "k", "summary": "comment on #1"
        }}});
        let meta = EffectMeta::read(Some(&result), None).unwrap();
        assert_eq!(meta.class, Some(EffectClass::Reversible));
        let undo = meta.undo.unwrap().unwrap();
        assert_eq!(undo.operation, "comments_delete");
        assert_eq!(undo.arguments, json!({"id": 7}));
        assert_eq!(undo.kind, UndoKind::Inverse);
        assert_eq!(undo.deadline_ms, Some(99));
        assert_eq!(meta.idempotency_key.as_deref(), Some("k"));
        let header = r#"{"class": "compensable", "undo": {"operation": "issues_close"}}"#;
        let meta = EffectMeta::read(Some(&json!({})), Some(header)).unwrap();
        assert_eq!(meta.undo.unwrap().unwrap().kind, UndoKind::Compensate);
        let none = EffectMeta::read(
            Some(&json!({"_meta": {"effect": {"class": "irreversible", "undo": null}}})),
            None,
        )
        .unwrap();
        assert_eq!(none.undo, Some(None));
        assert_eq!(EffectMeta::read(Some(&json!({})), None), None);
        assert_eq!(EffectMeta::read(None, Some("not json")), None);
    }

    #[test]
    fn tools_declare_their_class_or_are_the_worst_case() {
        let read = ToolDecl::read(
            &json!({"name": "g__issues_list", "annotations": {"readOnlyHint": true}}),
        );
        assert_eq!(read.class(), EffectClass::Read);
        let bare = ToolDecl::read(&json!({"name": "g__issues_create"}));
        assert_eq!(bare.class(), EffectClass::Irreversible);
        let declared = ToolDecl::read(&json!({"name": "m__send", "_meta": {"effect": {
            "class": "irreversible", "draft": true, "deletion": false, "operation": "mail.send",
            "lookup": {"operation": "sent_lookup"}}}}));
        assert!(declared.draft);
        assert_eq!(declared.operation.as_deref(), Some("mail.send"));
        assert_eq!(declared.lookup.unwrap().operation, "sent_lookup");
    }

    #[test]
    fn answers_come_from_json_or_an_event_stream() {
        let id = json!(3);
        let body = br#"{"jsonrpc":"2.0","id":3,"result":{"ok":true}}"#;
        assert_eq!(
            rpc_answer(Some("application/json"), body, &id).unwrap()["result"]["ok"],
            true
        );
        let sse = b"event: message\r\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\r\n\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":3,\r\ndata: \"result\":{\"n\":1}}\r\n\r\n";
        assert_eq!(
            rpc_answer(Some("text/event-stream"), sse, &id).unwrap()["result"]["n"],
            1
        );
        assert_eq!(rpc_answer(Some("application/json"), body, &json!(4)), None);
    }
}
