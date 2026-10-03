//! The gateway's wire as the ledger needs it, as Anvil serves it (ADR-0030,
//! Anvil's `docs/branchyard.md`, "Effects and undo"): MCP Streamable HTTP
//! (JSON-RPC over POST, answered with JSON or a server-sent event stream) and
//! the REST route `POST /call/<tool>`; the effect report each call returns
//! (`_meta.effect`, or the `X-Anvil-Effect` header); and the contract
//! `tools/list` publishes for each operation that declares one
//! (`_meta["anvil/effect_class"]`, `_meta["anvil/effect_contract"]`). See
//! `docs/effects.md#the-wire`.

use std::collections::HashMap;
use std::io::Read;

use serde_json::{json, Map, Value};

use super::{Draft, EffectClass, FollowUp, Lookup, Undo, UndoKind};
use crate::models::upstream::{self, Failure, Target};

/// The MCP revision Branchyard offers.
pub const PROTOCOL_VERSION: &str = "2025-06-18";
/// The header carrying the effect report on a REST answer.
pub const EFFECT_HEADER: &str = "X-Anvil-Effect";
/// Largest gateway answer read.
pub const MAX_ANSWER: usize = 16 * 1024 * 1024;

/// What the gateway reported about one call (Anvil's `EffectReport`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EffectMeta {
    pub class: Option<EffectClass>,
    /// The AIR operation that ran.
    pub operation: Option<String>,
    /// `Some(None)` when the gateway said there is no undo.
    pub undo: Option<Option<Undo>>,
    /// A reversible effect's compensation, for after the deadline.
    pub compensate: Option<Undo>,
    pub lookup: Option<Lookup>,
    /// Why `undo` is null although the class has one.
    pub undo_unavailable: Option<String>,
    /// The key that went upstream; `None` when none did.
    pub idempotency_key: Option<String>,
    /// A staged call's draft.
    pub staged: Option<Draft>,
}

fn follow_up(value: &Value) -> Option<FollowUp> {
    Some(FollowUp {
        operation: value.get("operation")?.as_str()?.to_owned(),
        tool: value.get("tool")?.as_str()?.to_owned(),
        arguments: value.get("arguments").cloned().unwrap_or(json!({})),
    })
}

fn undo_of(value: &Value, kind: Option<UndoKind>, deadline_ms: Option<u64>) -> Option<Undo> {
    let call = follow_up(value)?;
    let kind = match value.get("kind").and_then(Value::as_str) {
        Some("compensate") => UndoKind::Compensate,
        Some("inverse") => UndoKind::Inverse,
        _ => kind?,
    };
    Some(Undo {
        operation: call.operation,
        tool: call.tool,
        arguments: call.arguments,
        kind,
        deadline_ms: value
            .get("deadline_ms")
            .and_then(Value::as_u64)
            .or(deadline_ms),
    })
}

impl EffectMeta {
    /// From a `tools/call` result's `_meta.effect`, else the REST header.
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

    pub fn parse(value: &Value) -> EffectMeta {
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
        let deadline = value.get("deadline_ms").and_then(Value::as_u64);
        EffectMeta {
            class: value
                .get("class")
                .and_then(Value::as_str)
                .and_then(|c| EffectClass::parse(c).ok()),
            operation: text("operation"),
            undo: match value.get("undo") {
                None => None,
                Some(Value::Null) => Some(None),
                Some(undo) => Some(undo_of(undo, None, deadline)),
            },
            compensate: value
                .get("compensate")
                .filter(|c| c.is_object())
                .and_then(|c| undo_of(c, Some(UndoKind::Compensate), None)),
            lookup: value.get("lookup").filter(|l| l.is_object()).and_then(|l| {
                let call = follow_up(l)?;
                Some(Lookup {
                    operation: call.operation,
                    tool: call.tool,
                    by: l
                        .get("by")
                        .and_then(Value::as_str)
                        .unwrap_or("id")
                        .to_owned(),
                    arguments: call.arguments,
                })
            }),
            undo_unavailable: text("undo_unavailable"),
            idempotency_key: text("idempotency_key"),
            staged: value
                .get("staged")
                .filter(|s| s.is_object())
                .map(|s| Draft {
                    draft_operation: s
                        .get("draft_operation")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    handle: s.get("handle").cloned().unwrap_or(Value::Null),
                    promote: s.get("promote").and_then(follow_up),
                    discard: s.get("discard").and_then(follow_up),
                    unavailable: s
                        .get("unavailable")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                }),
        }
    }
}

/// What `tools/list` declares about a tool before it is called: its MCP
/// annotations, its AIR operation id (`anvil/operation_id`) and Anvil's
/// effect contract (`anvil/effect_class`, `anvil/effect_contract`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolDecl {
    /// The AIR operation id, when published.
    pub operation: Option<String>,
    pub class: Option<EffectClass>,
    pub read_only: bool,
    /// Whether the operation has a draft form (`stage: true`).
    pub draft: bool,
    /// The declared lookup: `{operation, by, arguments}`, its arguments a
    /// mapping from `request.*`, `idempotency_key` and `{const}`.
    pub lookup: Option<Value>,
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
        let meta = tool.get("_meta");
        let contract = meta.and_then(|m| m.get("anvil/effect_contract"));
        let class_of = |v: Option<&Value>| {
            v.and_then(Value::as_str)
                .and_then(|c| EffectClass::parse(c).ok())
        };
        ToolDecl {
            operation: meta
                .and_then(|m| m.get("anvil/operation_id"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            class: class_of(meta.and_then(|m| m.get("anvil/effect_class")))
                .or_else(|| class_of(contract.and_then(|c| c.get("class"))))
                .or_else(|| {
                    class_of(
                        meta.and_then(|m| m.get("effect"))
                            .and_then(|e| e.get("class")),
                    )
                }),
            read_only: hint("readOnlyHint"),
            draft: contract
                .and_then(|c| c.get("draft"))
                .is_some_and(Value::is_object),
            lookup: contract
                .and_then(|c| c.get("lookup"))
                .filter(|l| l.is_object())
                .cloned(),
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

/// One mapped value of a follow-up (Anvil's path grammar): `request.<path>`,
/// `idempotency_key`, or `{const}`. `Err` when it cannot be resolved before
/// the call's answer (`response.*`), `Ok(None)` to leave out an optional
/// argument that names nothing.
fn mapped(source: &Value, request: &Value, key: &str) -> Result<Option<Value>, ()> {
    if let Some(constant) = source.as_object().and_then(|o| o.get("const")) {
        return Ok(Some(constant.clone()));
    }
    let written = source.as_str().ok_or(())?;
    let optional = written.ends_with('?');
    let path = written.trim_end_matches('?');
    if path == "idempotency_key" {
        return Ok(Some(json!(key)));
    }
    let Some(rest) = path.strip_prefix("request.") else {
        return Err(());
    };
    let mut current = request;
    let mut found = true;
    for part in rest.split('.') {
        let (name, indices) = match part.find('[') {
            Some(i) => (&part[..i], &part[i..]),
            None => (part, ""),
        };
        current = match current.get(name) {
            Some(v) => v,
            None => {
                found = false;
                break;
            }
        };
        for index in indices
            .split(['[', ']'])
            .filter(|s| !s.is_empty())
            .filter_map(|n| n.parse::<usize>().ok())
        {
            current = match current.get(index) {
                Some(v) => v,
                None => {
                    found = false;
                    break;
                }
            };
        }
    }
    match (found, optional) {
        (true, _) => Ok(Some(current.clone())),
        (false, true) => Ok(None),
        (false, false) => Err(()),
    }
}

/// The declared lookup resolved before the call, from its arguments and the
/// entry's id (the key it carries), when the lookup operation's tool is
/// served (`tools`: AIR operation id to tool name). `None` when a value
/// comes only from the answer, or the tool is not known.
pub fn resolve_lookup(
    declared: &Value,
    request: &Value,
    key: &str,
    tools: &HashMap<String, String>,
) -> Option<Lookup> {
    let operation = declared.get("operation")?.as_str()?;
    let tool = tools.get(operation)?;
    let mut arguments = Map::new();
    if let Some(mapping) = declared.get("arguments").and_then(Value::as_object) {
        for (name, source) in mapping {
            if let Some(value) = mapped(source, request, key).ok()? {
                arguments.insert(name.clone(), value);
            }
        }
    }
    Some(Lookup {
        operation: operation.to_owned(),
        tool: tool.clone(),
        by: declared
            .get("by")
            .and_then(Value::as_str)
            .unwrap_or("id")
            .to_owned(),
        arguments: Value::Object(arguments),
    })
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

    /// A tool error's code (`not_found`, `unsupported_operation`, ...),
    /// from Anvil's `{"error": {"code"}}` envelope.
    pub fn error_code(&self) -> Option<String> {
        let result = self.result.as_ref()?;
        if self.ok() {
            return None;
        }
        let envelope = result.get("structuredContent").cloned().or_else(|| {
            result
                .get("content")?
                .as_array()?
                .iter()
                .find_map(|i| serde_json::from_str::<Value>(i.get("text")?.as_str()?).ok())
        })?;
        let error = envelope.get("error").unwrap_or(&envelope);
        error.get("code")?.as_str().map(str::to_owned)
    }

    /// What a lookup's answer says of the effect it looks for: found when
    /// it answered something, not found when it answered `not_found` or
    /// nothing, `None` when it cannot tell.
    pub fn found(&self) -> Option<bool> {
        if self.ok() {
            let result = self.result.as_ref()?;
            let data = result.get("structuredContent").cloned().or_else(|| {
                let text = result
                    .get("content")?
                    .as_array()?
                    .iter()
                    .find_map(|i| i.get("text")?.as_str().map(str::to_owned))?;
                Some(serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text)))
            });
            let empty = match &data {
                None | Some(Value::Null) => true,
                Some(Value::Array(a)) => a.is_empty(),
                Some(Value::Object(o)) => o.is_empty(),
                Some(Value::String(t)) => t.trim().is_empty() || t.trim() == "null",
                Some(_) => false,
            };
            return Some(!empty);
        }
        (self.error_code().as_deref() == Some("not_found")).then_some(false)
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

    /// Make a follow-up the gateway reported (an undo, a promotion, a
    /// discard, a lookup): `tool` with `arguments`, confirmed when its
    /// schema requires it, under `idempotency` when given.
    pub fn follow_up(
        &self,
        tool: &str,
        arguments: &Value,
        idempotency: Option<&str>,
    ) -> Result<Called, CallError> {
        let listed = self.list_tools().map_err(CallError::NotSent)?;
        let declared = listed
            .iter()
            .find(|t| t.get("name").and_then(Value::as_str) == Some(tool));
        let arguments = confirmed(declared, arguments);
        let meta = match idempotency {
            Some(key) => json!({"idempotency_key": key}),
            None => json!({}),
        };
        self.call(tool, &arguments, &meta, idempotency)
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

/// A REST call's answer (`POST /call/<tool>`) from its HTTP response, in
/// the shape of a `tools/call` answer: `200` with the data, or an
/// `{"error": {...}}` envelope with a status for its code; the effect report
/// in the `X-Anvil-Effect` header.
pub fn rest_called(response: &upstream::Response, body: &[u8]) -> Called {
    let parsed: Option<Value> = serde_json::from_slice(body).ok();
    let text = String::from_utf8_lossy(&body[..body.len().min(MAX_ANSWER)]).into_owned();
    let ok = (200..300).contains(&response.status);
    let envelope = parsed
        .as_ref()
        .and_then(|p| p.get("error"))
        .filter(|e| e.is_object());
    let answered = ok || envelope.is_some();
    let result = answered.then(|| {
        let mut result = json!({"content": [{"type": "text", "text": text}], "isError": !ok});
        if let Some(data) = &parsed {
            result["structuredContent"] = data.clone();
        }
        result
    });
    Called {
        status: response.status,
        answered,
        result,
        error: (!answered).then(|| {
            format!(
                "{} {}",
                response.status,
                String::from_utf8_lossy(&body[..body.len().min(300)])
            )
        }),
        meta: EffectMeta::read(None, response.header(EFFECT_HEADER)),
    }
}

/// Fill in the confirmation a follow-up's tool requires, under the key its
/// input schema names (a required boolean that must be `true`; `confirm`,
/// or `anvil_confirm` when the operation takes a `confirm` of its own). A
/// follow-up Branchyard makes was approved by its own policy.
pub fn confirmed(tool: Option<&Value>, arguments: &Value) -> Value {
    let mut arguments = match arguments {
        Value::Object(_) => arguments.clone(),
        _ => json!({}),
    };
    let Some(schema) = tool.and_then(|t| t.get("inputSchema")) else {
        return arguments;
    };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (name, property) in properties {
            let confirm = property.get("type").and_then(Value::as_str) == Some("boolean")
                && property.get("const") == Some(&Value::Bool(true))
                && required.contains(&name.as_str());
            if confirm && arguments.get(name).is_none() {
                arguments[name] = json!(true);
            }
        }
    }
    arguments
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
    fn effect_reports_are_read_from_meta_or_the_header() {
        let result = json!({"content": [], "_meta": {"effect": {
            "class": "reversible", "operation": "createIssueComment",
            "undo": {"kind": "inverse", "operation": "deleteIssueComment",
                     "tool": "github__deleteIssueComment", "arguments": {"id": 7}},
            "deadline_ms": 99, "idempotency_key": "k",
            "compensate": {"kind": "compensate", "operation": "updateIssueComment",
                           "tool": "github__updateIssueComment", "arguments": {"id": 7},
                           "deadline_ms": 500},
            "lookup": {"by": "id", "operation": "getIssueComment",
                       "tool": "github__getIssueComment", "arguments": {"id": 7}}
        }}});
        let meta = EffectMeta::read(Some(&result), None).unwrap();
        assert_eq!(meta.class, Some(EffectClass::Reversible));
        assert_eq!(meta.operation.as_deref(), Some("createIssueComment"));
        let undo = meta.undo.unwrap().unwrap();
        assert_eq!(undo.tool, "github__deleteIssueComment");
        assert_eq!(undo.arguments, json!({"id": 7}));
        assert_eq!(undo.kind, UndoKind::Inverse);
        assert_eq!(undo.deadline_ms, Some(99));
        let compensate = meta.compensate.unwrap();
        assert_eq!(compensate.kind, UndoKind::Compensate);
        assert_eq!(compensate.deadline_ms, Some(500));
        assert_eq!(meta.lookup.unwrap().tool, "github__getIssueComment");
        assert_eq!(meta.idempotency_key.as_deref(), Some("k"));
        let header = r#"{"class": "irreversible", "operation": "createRelease", "undo": null,
            "idempotency_key": null, "lookup": null,
            "staged": {"draft_operation": "createRelease", "handle": 12,
              "promote": {"operation": "updateRelease", "tool": "github__updateRelease",
                          "arguments": {"release_id": 12, "draft": false}},
              "discard": null}}"#;
        let meta = EffectMeta::read(Some(&json!({})), Some(header)).unwrap();
        assert_eq!(meta.undo, Some(None));
        let staged = meta.staged.unwrap();
        assert_eq!(staged.handle, json!(12));
        assert_eq!(staged.promote.unwrap().tool, "github__updateRelease");
        assert_eq!(staged.discard, None);
        let unavailable = EffectMeta::parse(&json!({"class": "reversible", "undo": null,
            "undo_unavailable": "response.id missing"}));
        assert_eq!(
            unavailable.undo_unavailable.as_deref(),
            Some("response.id missing")
        );
        assert_eq!(EffectMeta::read(Some(&json!({})), None), None);
        assert_eq!(EffectMeta::read(None, Some("not json")), None);
    }

    #[test]
    fn tools_declare_their_contract_or_are_the_worst_case() {
        let read = ToolDecl::read(
            &json!({"name": "g__issues_list", "annotations": {"readOnlyHint": true}}),
        );
        assert_eq!(read.class(), EffectClass::Read);
        let bare = ToolDecl::read(&json!({"name": "g__issues_create"}));
        assert_eq!(bare.class(), EffectClass::Irreversible);
        let fallback =
            ToolDecl::read(&json!({"name": "g__x", "_meta": {"effect": {"class": "reversible"}}}));
        assert_eq!(fallback.class(), EffectClass::Reversible);
        let declared = ToolDecl::read(&json!({"name": "github__createRelease", "_meta": {
            "anvil/operation_id": "createRelease",
            "anvil/effect_class": "irreversible",
            "anvil/effect_contract": {"class": "irreversible",
                "lookup": {"operation": "getReleaseByTag", "by": "id",
                           "arguments": {"tag": "request.tag_name", "repo": {"const": "r"}}},
                "draft": {"operation": "createRelease", "arguments": {"draft": {"const": true}},
                          "handle": "response.id",
                          "promote": {"operation": "updateRelease", "arguments": {}}}}}}));
        assert!(declared.draft);
        assert_eq!(declared.class(), EffectClass::Irreversible);
        assert_eq!(declared.operation.as_deref(), Some("createRelease"));
        let tools = HashMap::from([(
            "getReleaseByTag".to_owned(),
            "github__getReleaseByTag".to_owned(),
        )]);
        let contract = declared.lookup.as_ref().unwrap();
        let lookup = resolve_lookup(contract, &json!({"tag_name": "v1"}), "key-1", &tools).unwrap();
        assert_eq!(lookup.tool, "github__getReleaseByTag");
        assert_eq!(lookup.arguments, json!({"tag": "v1", "repo": "r"}));
        assert_eq!(
            resolve_lookup(contract, &json!({}), "k", &tools),
            None,
            "a required request value that is missing"
        );
        assert_eq!(
            resolve_lookup(contract, &json!({"tag_name": "v1"}), "k", &HashMap::new()),
            None,
            "the lookup's tool is not served"
        );
    }

    #[test]
    fn mappings_follow_anvils_path_grammar() {
        let request = json!({"a": {"b": [{"c": 4}]}});
        assert_eq!(
            mapped(&json!("request.a.b[0].c"), &request, "k"),
            Ok(Some(json!(4)))
        );
        assert_eq!(mapped(&json!("request.a.z?"), &request, "k"), Ok(None));
        assert_eq!(mapped(&json!("request.a.z"), &request, "k"), Err(()));
        assert_eq!(
            mapped(&json!("idempotency_key"), &request, "k"),
            Ok(Some(json!("k")))
        );
        assert_eq!(
            mapped(&json!({"const": "closed"}), &request, "k"),
            Ok(Some(json!("closed")))
        );
        assert_eq!(mapped(&json!("response.id"), &request, "k"), Err(()));
    }

    #[test]
    fn follow_ups_are_confirmed_when_their_schema_requires_it() {
        let tool = json!({"name": "g__close", "inputSchema": {"type": "object",
            "properties": {"id": {"type": "integer"}, "anvil_confirm": {"type": "boolean", "const": true},
                           "confirm": {"type": "boolean"}},
            "required": ["id", "anvil_confirm"]}});
        assert_eq!(
            confirmed(Some(&tool), &json!({"id": 1, "confirm": false})),
            json!({"id": 1, "confirm": false, "anvil_confirm": true})
        );
        assert_eq!(confirmed(None, &json!({"id": 1})), json!({"id": 1}));
    }

    #[test]
    fn lookups_tell_found_from_not_found() {
        let answer = |result: Value| Called {
            status: 200,
            answered: true,
            result: Some(result),
            error: None,
            meta: None,
        };
        let found = answer(json!({"content": [{"type": "text", "text": "{\"id\": 3}"}]}));
        assert_eq!(found.found(), Some(true));
        let empty = answer(json!({"content": [{"type": "text", "text": "[]"}]}));
        assert_eq!(empty.found(), Some(false));
        let missing = answer(json!({"isError": true, "content": [{"type": "text",
            "text": "{\"error\": {\"code\": \"not_found\", \"message\": \"no\"}}"}]}));
        assert_eq!(missing.error_code().as_deref(), Some("not_found"));
        assert_eq!(missing.found(), Some(false));
        let failing = answer(json!({"isError": true, "content": [{"type": "text",
            "text": "{\"error\": {\"code\": \"rate_limited\"}}"}]}));
        assert_eq!(failing.found(), None);
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
