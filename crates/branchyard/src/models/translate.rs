//! Anthropic's Messages API on an OpenAI chat-completions backend: the
//! request translated on its way out, the response (streamed or not) on
//! its way back, so a harness that speaks Anthropic's API (Claude Code)
//! can be routed to any backend that speaks OpenAI's (`docs/model-gateway.md`,
//! "Translation"). Coder's anyclaude (MIT, vendored under
//! `vendor/anyclaude`) is the reference for what maps to what.
//!
//! What maps:
//!
//! - `system` (text, or text blocks) to a `system` message; `messages`
//!   with text, image (`image_url`, a data URL for base64 sources),
//!   `tool_use` (`tool_calls`) and `tool_result` (`tool` messages, placed
//!   before the rest of their user turn, as OpenAI requires) blocks;
//!   `thinking` blocks on the way out are dropped, as anyclaude does.
//! - `tools` with an `input_schema` to functions; server tools (no schema)
//!   are dropped. `tool_choice` `auto`, `any` (`required`), `none` and
//!   `tool` (that function); `disable_parallel_tool_use` to
//!   `parallel_tool_calls: false`.
//! - `max_tokens` to `max_completion_tokens`, `stop_sequences` to `stop`,
//!   `temperature`, `top_p`, `stream` (with `stream_options.include_usage`,
//!   so usage comes back), `metadata.user_id` to `user`; `thinking` to
//!   `reasoning_effort` by its budget (`low` under 4096 tokens, `medium`
//!   under 16384, else `high`). `top_k` has no counterpart and is dropped.
//! - A completion's `content` to a text block, `tool_calls` to `tool_use`
//!   blocks (arguments parsed as JSON), `reasoning_content` (DeepSeek,
//!   vLLM) to a `thinking` block; `finish_reason` to `stop_reason`
//!   (`stop` is `end_turn`, `length` is `max_tokens`, `tool_calls` is
//!   `tool_use`, `content_filter` is `refusal`); `usage` to Anthropic's,
//!   cached prompt tokens as `cache_read_input_tokens`.
//! - A stream of chunks to Anthropic's events: `message_start`, a
//!   `content_block_start`/`delta`/`stop` per text, thinking or tool-call
//!   block, `message_delta` with the stop reason and usage, `message_stop`.
//! - An error body to Anthropic's `{"type": "error", "error": {...}}`,
//!   the status kept.
//!
//! What does not: documents (PDF) and unknown block types refuse the
//! request; `/v1/messages/count_tokens` and the rest of Anthropic's API are
//! not translated; a `thinking` block comes back without a signature, so
//! it cannot be sent back to Anthropic later.

use std::collections::HashMap;

use serde_json::{json, Map, Value};

/// The chat-completions path on an OpenAI backend.
pub(crate) const CHAT_PATH: &str = "/v1/chat/completions";

/// Whether a request to Anthropic's API can be translated: `POST
/// /v1/messages`, nothing else.
pub(crate) fn translatable(method: &str, path: &str) -> bool {
    method == "POST" && path.split('?').next() == Some("/v1/messages")
}

fn text_of(content: &Value) -> Result<String, String> {
    match content {
        Value::String(text) => Ok(text.clone()),
        Value::Array(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => parts.push(
                        block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    ),
                    Some("image") => parts.push("[image]".to_owned()),
                    other => return Err(format!("a {} block has no text", other.unwrap_or("?"))),
                }
            }
            Ok(parts.join("\n"))
        }
        Value::Null => Ok(String::new()),
        _ => Err("content is neither text nor blocks".into()),
    }
}

/// An image block as OpenAI's `image_url` part.
fn image_part(block: &Value) -> Result<Value, String> {
    let source = &block["source"];
    let url = match source.get("type").and_then(Value::as_str) {
        Some("base64") => format!(
            "data:{};base64,{}",
            source["media_type"].as_str().unwrap_or("image/jpeg"),
            source["data"].as_str().unwrap_or_default()
        ),
        Some("url") => source["url"].as_str().unwrap_or_default().to_owned(),
        other => {
            return Err(format!(
                "an image with a {} source cannot be translated",
                other.unwrap_or("?")
            ))
        }
    };
    Ok(json!({"type": "image_url", "image_url": {"url": url}}))
}

/// A user turn's blocks: `tool` messages first, then one user message of
/// what is left.
fn user_turn(blocks: &[Value], out: &mut Vec<Value>) -> Result<(), String> {
    let mut parts: Vec<Value> = Vec::new();
    let mut all_text = true;
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("tool_result") => {
                let content = text_of(block.get("content").unwrap_or(&Value::Null))?;
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": block["tool_use_id"].as_str().unwrap_or_default(),
                    "content": content,
                }));
            }
            Some("text") => parts.push(json!({
                "type": "text",
                "text": block["text"].as_str().unwrap_or_default(),
            })),
            Some("image") => {
                all_text = false;
                parts.push(image_part(block)?);
            }
            Some("document") => return Err("a document block cannot be translated".into()),
            other => {
                return Err(format!(
                    "a {} block in a user message cannot be translated",
                    other.unwrap_or("?")
                ))
            }
        }
    }
    if parts.is_empty() {
        return Ok(());
    }
    let content = match all_text {
        true => Value::String(
            parts
                .iter()
                .map(|p| p["text"].as_str().unwrap_or_default())
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        false => Value::Array(parts),
    };
    out.push(json!({"role": "user", "content": content}));
    Ok(())
}

/// An assistant turn's blocks: its text, and its tool uses as calls.
fn assistant_turn(blocks: &[Value], out: &mut Vec<Value>) -> Result<(), String> {
    let mut text: Vec<&str> = Vec::new();
    let mut calls = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => text.push(block["text"].as_str().unwrap_or_default()),
            Some("tool_use") => calls.push(json!({
                "id": block["id"].as_str().unwrap_or_default(),
                "type": "function",
                "function": {
                    "name": block["name"].as_str().unwrap_or_default(),
                    "arguments": block.get("input").unwrap_or(&json!({})).to_string(),
                },
            })),
            // The backend never saw its own reasoning in Anthropic's shape.
            Some("thinking") | Some("redacted_thinking") => {}
            other => {
                return Err(format!(
                    "a {} block in an assistant message cannot be translated",
                    other.unwrap_or("?")
                ))
            }
        }
    }
    let mut message = Map::new();
    message.insert("role".into(), json!("assistant"));
    let content = match text.is_empty() {
        true => Value::Null,
        false => Value::String(text.join("")),
    };
    message.insert("content".into(), content);
    if !calls.is_empty() {
        message.insert("tool_calls".into(), Value::Array(calls));
    }
    out.push(Value::Object(message));
    Ok(())
}

/// The chat-completions request for an Anthropic Messages `request`,
/// asking the backend for `model`.
pub(crate) fn request_to_chat(request: &Value, model: &str) -> Result<Value, String> {
    let object = request
        .as_object()
        .ok_or("the request body is not a JSON object")?;
    let mut messages: Vec<Value> = Vec::new();
    if let Some(system) = object.get("system") {
        let text = text_of(system).map_err(|e| format!("system: {e}"))?;
        if !text.is_empty() {
            messages.push(json!({"role": "system", "content": text}));
        }
    }
    let turns = object
        .get("messages")
        .and_then(Value::as_array)
        .ok_or("the request has no messages")?;
    for turn in turns {
        let role = turn["role"].as_str().unwrap_or_default();
        match (role, &turn["content"]) {
            (_, Value::String(text)) if role == "user" || role == "assistant" => {
                messages.push(json!({"role": role, "content": text}));
            }
            ("user", Value::Array(blocks)) => user_turn(blocks, &mut messages)?,
            ("assistant", Value::Array(blocks)) => assistant_turn(blocks, &mut messages)?,
            // Claude Code 2.1.301 put a message with role "system" (text, or
            // text blocks) among its messages, which Anthropic's API does
            // not document; a chat completion takes a system message in
            // place, so it goes through as one (seen 16 October 2026,
            // Claude Code on GLM through a zai backend).
            ("system", content) => {
                let text = text_of(content).map_err(|e| format!("a system message: {e}"))?;
                messages.push(json!({"role": "system", "content": text}));
            }
            _ => return Err(format!("a message with role {role:?} cannot be translated")),
        }
    }
    let mut chat = Map::new();
    chat.insert("model".into(), json!(model));
    chat.insert("messages".into(), Value::Array(messages));
    if let Some(max) = object.get("max_tokens") {
        chat.insert("max_completion_tokens".into(), max.clone());
    }
    for name in ["temperature", "top_p"] {
        if let Some(value) = object.get(name) {
            chat.insert(name.into(), value.clone());
        }
    }
    if let Some(stops) = object
        .get("stop_sequences")
        .filter(|s| !s.as_array().is_some_and(Vec::is_empty))
    {
        chat.insert("stop".into(), stops.clone());
    }
    if let Some(user) = object.get("metadata").and_then(|m| m.get("user_id")) {
        chat.insert("user".into(), user.clone());
    }
    if object.get("stream") == Some(&Value::Bool(true)) {
        chat.insert("stream".into(), json!(true));
        chat.insert("stream_options".into(), json!({"include_usage": true}));
    }
    if let Some(thinking) = object.get("thinking") {
        if thinking.get("type").and_then(Value::as_str) == Some("enabled") {
            let budget = thinking["budget_tokens"].as_u64().unwrap_or(0);
            let effort = match budget {
                0..4096 => "low",
                4096..16384 => "medium",
                _ => "high",
            };
            chat.insert("reasoning_effort".into(), json!(effort));
        }
    }
    let tools: Vec<Value> = object
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter(|t| t.get("input_schema").is_some())
                .map(|t| {
                    let mut function = Map::new();
                    function.insert("name".into(), t["name"].clone());
                    if let Some(description) = t.get("description") {
                        function.insert("description".into(), description.clone());
                    }
                    function.insert("parameters".into(), t["input_schema"].clone());
                    json!({"type": "function", "function": Value::Object(function)})
                })
                .collect()
        })
        .unwrap_or_default();
    if !tools.is_empty() {
        chat.insert("tools".into(), Value::Array(tools));
        if let Some(choice) = object.get("tool_choice") {
            let translated = match choice.get("type").and_then(Value::as_str) {
                Some("auto") | None => json!("auto"),
                Some("any") => json!("required"),
                Some("none") => json!("none"),
                Some("tool") => json!({
                    "type": "function",
                    "function": {"name": choice["name"].as_str().unwrap_or_default()},
                }),
                Some(other) => return Err(format!("tool_choice {other:?} cannot be translated")),
            };
            chat.insert("tool_choice".into(), translated);
            if choice.get("disable_parallel_tool_use") == Some(&Value::Bool(true)) {
                chat.insert("parallel_tool_calls".into(), json!(false));
            }
        }
    }
    Ok(Value::Object(chat))
}

/// Anthropic's stop reason for a completion's finish reason.
fn stop_reason(finish: Option<&str>, called_tools: bool) -> &'static str {
    match finish {
        Some("tool_calls") | Some("function_call") => "tool_use",
        Some("length") => "max_tokens",
        Some("content_filter") => "refusal",
        Some("stop") => "end_turn",
        _ if called_tools => "tool_use",
        _ => "end_turn",
    }
}

/// Anthropic's usage for a completion's.
fn usage_of(usage: &Value) -> Value {
    let prompt = usage["prompt_tokens"].as_u64().unwrap_or(0);
    let cached = usage["prompt_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or(0)
        .min(prompt);
    json!({
        "input_tokens": prompt - cached,
        "output_tokens": usage["completion_tokens"].as_u64().unwrap_or(0),
        "cache_read_input_tokens": cached,
        "cache_creation_input_tokens": 0,
    })
}

/// A tool call's `input`: its arguments parsed, or an object holding the
/// text when they are not JSON.
fn tool_input(arguments: &Value) -> Value {
    match arguments {
        Value::String(text) if text.trim().is_empty() => json!({}),
        Value::String(text) => {
            serde_json::from_str(text).unwrap_or_else(|_| json!({"arguments": text}))
        }
        Value::Object(_) => arguments.clone(),
        _ => json!({}),
    }
}

/// An Anthropic message for a chat completion `response` (not streamed),
/// said to be from `model`.
pub(crate) fn response_to_message(response: &Value, model: &str) -> Result<Value, String> {
    let choice = response["choices"]
        .as_array()
        .and_then(|c| c.first())
        .ok_or("the backend answered with no choices")?;
    let message = &choice["message"];
    let mut content = Vec::new();
    if let Some(reasoning) = message
        .get("reasoning_content")
        .or_else(|| message.get("reasoning"))
        .and_then(Value::as_str)
        .filter(|r| !r.is_empty())
    {
        content.push(json!({"type": "thinking", "thinking": reasoning}));
    }
    if let Some(text) = message["content"].as_str().filter(|t| !t.is_empty()) {
        content.push(json!({"type": "text", "text": text}));
    }
    let calls = message["tool_calls"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for call in &calls {
        content.push(json!({
            "type": "tool_use",
            "id": call["id"].as_str().unwrap_or_default(),
            "name": call["function"]["name"].as_str().unwrap_or_default(),
            "input": tool_input(&call["function"]["arguments"]),
        }));
    }
    let finish = choice["finish_reason"].as_str();
    Ok(json!({
        "id": message_id(response["id"].as_str()),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason(finish, !calls.is_empty()),
        "stop_sequence": Value::Null,
        "usage": usage_of(&response["usage"]),
    }))
}

fn message_id(backend: Option<&str>) -> String {
    match backend {
        Some(id) if !id.is_empty() => format!("msg_{id}"),
        _ => format!("msg_{}", branchyard_support::time::now_ms()),
    }
}

/// Anthropic's error type for a status.
fn error_type(status: u16) -> &'static str {
    match status {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        529 => "overloaded_error",
        _ => "api_error",
    }
}

/// An Anthropic error body for a chat backend's error answer: its message
/// kept, its type by the status.
pub(crate) fn error_to_anthropic(status: u16, body: &[u8]) -> Value {
    let parsed: Option<Value> = serde_json::from_slice(body).ok();
    let message = parsed
        .as_ref()
        .and_then(|v| {
            v["error"]["message"]
                .as_str()
                .or_else(|| v["error"].as_str())
                .or_else(|| v["message"].as_str())
        })
        .map_or_else(
            || match String::from_utf8_lossy(body).trim() {
                "" => format!("the backend answered {status}"),
                text => text.chars().take(2000).collect(),
            },
            str::to_owned,
        );
    json!({
        "type": "error",
        "error": {"type": error_type(status), "message": message},
    })
}

/// The block a stream has open.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Open {
    None,
    Text,
    Thinking,
    /// A tool call, by the backend's index.
    Tool(u64),
}

/// Translates a chat-completions stream into Anthropic's events as its
/// chunks arrive. Feed it the backend's bytes; write what it returns.
pub(crate) struct StreamTranslator {
    model: String,
    line: Vec<u8>,
    data: String,
    started: bool,
    ended: bool,
    /// The next content block's index.
    index: u64,
    open: Open,
    /// The backend's tool call index to the block that carries it.
    tools: HashMap<u64, u64>,
    finish: Option<String>,
    called_tools: bool,
    usage: Option<Value>,
}

impl StreamTranslator {
    pub(crate) fn new(model: &str) -> StreamTranslator {
        StreamTranslator {
            model: model.to_owned(),
            line: Vec::new(),
            data: String::new(),
            started: false,
            ended: false,
            index: 0,
            open: Open::None,
            tools: HashMap::new(),
            finish: None,
            called_tools: false,
            usage: None,
        }
    }

    /// The Anthropic events for the backend's `bytes`, so far.
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for &byte in bytes {
            if byte == b'\n' {
                let line = std::mem::take(&mut self.line);
                self.line_done(&line, &mut out);
            } else {
                self.line.push(byte);
            }
        }
        out
    }

    /// The end of the backend's stream: what it left open is closed, and
    /// the message ended if the backend did not end it.
    pub(crate) fn finish(mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.line.is_empty() {
            let line = std::mem::take(&mut self.line);
            self.line_done(&line, &mut out);
        }
        if !self.data.is_empty() {
            let data = std::mem::take(&mut self.data);
            self.event(&data, &mut out);
        }
        if self.started {
            self.end(&mut out);
        }
        out
    }

    fn line_done(&mut self, line: &[u8], out: &mut Vec<u8>) {
        let line = String::from_utf8_lossy(line);
        let line = line.strip_suffix('\r').unwrap_or(&line);
        if line.is_empty() {
            let data = std::mem::take(&mut self.data);
            if !data.is_empty() {
                self.event(&data, out);
            }
            return;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            if !self.data.is_empty() {
                self.data.push('\n');
            }
            self.data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }

    fn emit(out: &mut Vec<u8>, kind: &str, event: &Value) {
        out.extend_from_slice(format!("event: {kind}\ndata: {event}\n\n").as_bytes());
    }

    fn start(&mut self, chunk: &Value, out: &mut Vec<u8>) {
        if self.started {
            return;
        }
        self.started = true;
        Self::emit(
            out,
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": message_id(chunk["id"].as_str()),
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": {"input_tokens": 0, "output_tokens": 0},
                },
            }),
        );
    }

    fn close(&mut self, out: &mut Vec<u8>) {
        if self.open == Open::None {
            return;
        }
        Self::emit(
            out,
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": self.index}),
        );
        self.open = Open::None;
        self.index += 1;
    }

    fn open_block(&mut self, open: Open, block: Value, out: &mut Vec<u8>) {
        self.close(out);
        Self::emit(
            out,
            "content_block_start",
            &json!({"type": "content_block_start", "index": self.index, "content_block": block}),
        );
        self.open = open;
    }

    fn delta(&self, delta: Value, out: &mut Vec<u8>) {
        Self::emit(
            out,
            "content_block_delta",
            &json!({"type": "content_block_delta", "index": self.index, "delta": delta}),
        );
    }

    fn end(&mut self, out: &mut Vec<u8>) {
        if self.ended {
            return;
        }
        self.ended = true;
        self.close(out);
        let usage = self
            .usage
            .as_ref()
            .map_or_else(|| json!({"output_tokens": 0}), usage_of);
        Self::emit(
            out,
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": stop_reason(self.finish.as_deref(), self.called_tools),
                    "stop_sequence": Value::Null,
                },
                "usage": usage,
            }),
        );
        Self::emit(out, "message_stop", &json!({"type": "message_stop"}));
    }

    /// One of the backend's events.
    fn event(&mut self, data: &str, out: &mut Vec<u8>) {
        if data.trim() == "[DONE]" {
            if self.started {
                self.end(out);
            }
            return;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            return;
        };
        if let Some(error) = chunk.get("error") {
            let message = error["message"]
                .as_str()
                .or_else(|| error.as_str())
                .unwrap_or("the backend reported an error");
            Self::emit(
                out,
                "error",
                &json!({"type": "error", "error": {"type": "api_error", "message": message}}),
            );
            return;
        }
        if self.ended {
            return;
        }
        self.start(&chunk, out);
        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(usage.clone());
        }
        let Some(choice) = chunk["choices"].as_array().and_then(|c| c.first()) else {
            return;
        };
        let delta = &choice["delta"];
        if let Some(reasoning) = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(Value::as_str)
            .filter(|r| !r.is_empty())
        {
            if self.open != Open::Thinking {
                self.open_block(
                    Open::Thinking,
                    json!({"type": "thinking", "thinking": ""}),
                    out,
                );
            }
            self.delta(
                json!({"type": "thinking_delta", "thinking": reasoning}),
                out,
            );
        }
        if let Some(text) = delta["content"].as_str().filter(|t| !t.is_empty()) {
            if self.open != Open::Text {
                self.open_block(Open::Text, json!({"type": "text", "text": ""}), out);
            }
            self.delta(json!({"type": "text_delta", "text": text}), out);
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for (n, call) in calls.iter().enumerate() {
                let at = call["index"].as_u64().unwrap_or(n as u64);
                if self.open != Open::Tool(at) {
                    if self.tools.contains_key(&at) {
                        // A call the backend came back to after another
                        // block: its arguments continue in a new block of
                        // the same call, which a reader joins by id.
                        self.tools.remove(&at);
                    }
                    let id = match call["id"].as_str().filter(|i| !i.is_empty()) {
                        Some(id) => id.to_owned(),
                        None => format!("toolu_{}", self.index),
                    };
                    self.open_block(
                        Open::Tool(at),
                        json!({
                            "type": "tool_use",
                            "id": id,
                            "name": call["function"]["name"].as_str().unwrap_or_default(),
                            "input": {},
                        }),
                        out,
                    );
                    self.tools.insert(at, self.index);
                    self.called_tools = true;
                }
                if let Some(arguments) = call["function"]["arguments"]
                    .as_str()
                    .filter(|a| !a.is_empty())
                {
                    self.delta(
                        json!({"type": "input_json_delta", "partial_json": arguments}),
                        out,
                    );
                }
            }
        }
        if let Some(finish) = choice["finish_reason"].as_str() {
            self.finish = Some(finish.to_owned());
            self.close(out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(bytes: &[u8]) -> Vec<(String, Value)> {
        String::from_utf8_lossy(bytes)
            .split("\n\n")
            .filter(|e| !e.is_empty())
            .map(|e| {
                let mut lines = e.lines();
                let kind = lines
                    .next()
                    .and_then(|l| l.strip_prefix("event: "))
                    .unwrap()
                    .to_owned();
                let data = lines.next().and_then(|l| l.strip_prefix("data: ")).unwrap();
                (kind, serde_json::from_str(data).unwrap())
            })
            .collect()
    }

    #[test]
    fn a_messages_request_becomes_a_chat_completion() {
        let request = json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 1024,
            "temperature": 0.2,
            "top_p": 0.9,
            "top_k": 40,
            "stop_sequences": ["END"],
            "metadata": {"user_id": "u1"},
            "system": [{"type": "text", "text": "Be brief."}, {"type": "text", "text": "Be kind."}],
            "thinking": {"type": "enabled", "budget_tokens": 8000},
            "tools": [
                {"name": "read", "description": "Read a file", "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}},
                {"type": "web_search_20250305", "name": "web_search"}
            ],
            "tool_choice": {"type": "any", "disable_parallel_tool_use": true},
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "hmm", "signature": "x"},
                    {"type": "text", "text": "I will read it."},
                    {"type": "tool_use", "id": "toolu_1", "name": "read", "input": {"path": "a.txt"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": [{"type": "text", "text": "one"}, {"type": "text", "text": "two"}]},
                    {"type": "text", "text": "now this"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "QUJD"}}
                ]}
            ]
        });
        let chat = request_to_chat(&request, "gpt-5").unwrap();
        assert_eq!(chat["model"], "gpt-5");
        assert_eq!(chat["max_completion_tokens"], 1024);
        assert_eq!(chat["temperature"], 0.2);
        assert_eq!(chat["top_p"], 0.9);
        assert!(chat.get("top_k").is_none());
        assert_eq!(chat["stop"], json!(["END"]));
        assert_eq!(chat["user"], "u1");
        assert_eq!(chat["reasoning_effort"], "medium");
        assert!(chat.get("stream").is_none());
        let messages = chat["messages"].as_array().unwrap();
        assert_eq!(
            messages[0],
            json!({"role": "system", "content": "Be brief.\nBe kind."})
        );
        assert_eq!(messages[1], json!({"role": "user", "content": "hi"}));
        // A message with role "system" among the turns (Claude Code sends
        // one) is a system message in place, as text or text blocks.
        let mid = request_to_chat(
            &json!({"model": "m", "messages": [
                {"role": "user", "content": "hi"},
                {"role": "system", "content": [{"type": "text", "text": "Note A."}, {"type": "text", "text": "Note B."}]},
                {"role": "system", "content": "Note C."},
                {"role": "user", "content": "go"}
            ]}),
            "m",
        )
        .unwrap();
        assert_eq!(
            mid["messages"],
            json!([
                {"role": "user", "content": "hi"},
                {"role": "system", "content": "Note A.\nNote B."},
                {"role": "system", "content": "Note C."},
                {"role": "user", "content": "go"}
            ])
        );
        assert_eq!(
            messages[2],
            json!({"role": "assistant", "content": "I will read it.", "tool_calls": [
                {"id": "toolu_1", "type": "function", "function": {"name": "read", "arguments": "{\"path\":\"a.txt\"}"}}
            ]})
        );
        assert_eq!(
            messages[3],
            json!({"role": "tool", "tool_call_id": "toolu_1", "content": "one\ntwo"})
        );
        assert_eq!(messages[4]["role"], "user");
        let parts = messages[4]["content"].as_array().unwrap();
        assert_eq!(parts[0], json!({"type": "text", "text": "now this"}));
        assert_eq!(
            parts[1],
            json!({"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}})
        );
        assert_eq!(messages.len(), 5);
        let tools = chat["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1, "the server tool is dropped");
        assert_eq!(tools[0]["function"]["name"], "read");
        assert_eq!(tools[0]["function"]["description"], "Read a file");
        assert_eq!(tools[0]["function"]["parameters"]["type"], "object");
        assert_eq!(chat["tool_choice"], "required");
        assert_eq!(chat["parallel_tool_calls"], false);

        // The other tool choices, a streamed request, and what is refused.
        let mut streamed = json!({"model": "m", "max_tokens": 1, "stream": true,
            "system": "s", "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "t", "input_schema": {}}], "tool_choice": {"type": "tool", "name": "t"}});
        let chat = request_to_chat(&streamed, "gpt-5").unwrap();
        assert_eq!(chat["stream"], true);
        assert_eq!(chat["stream_options"], json!({"include_usage": true}));
        assert_eq!(
            chat["messages"][0],
            json!({"role": "system", "content": "s"})
        );
        assert_eq!(
            chat["tool_choice"],
            json!({"type": "function", "function": {"name": "t"}})
        );
        streamed["tool_choice"] = json!({"type": "none"});
        assert_eq!(
            request_to_chat(&streamed, "m").unwrap()["tool_choice"],
            "none"
        );
        streamed["tool_choice"] = json!({"type": "auto"});
        assert_eq!(
            request_to_chat(&streamed, "m").unwrap()["tool_choice"],
            "auto"
        );
        streamed["thinking"] = json!({"type": "enabled", "budget_tokens": 1024});
        assert_eq!(
            request_to_chat(&streamed, "m").unwrap()["reasoning_effort"],
            "low"
        );
        streamed["thinking"] = json!({"type": "enabled", "budget_tokens": 32000});
        assert_eq!(
            request_to_chat(&streamed, "m").unwrap()["reasoning_effort"],
            "high"
        );
        for (bad, why) in [
            (
                json!({"model": "m", "messages": [{"role": "user", "content": [{"type": "document", "source": {}}]}]}),
                "document",
            ),
            (
                json!({"model": "m", "messages": [{"role": "tool", "content": "x"}]}),
                "role \"tool\"",
            ),
            (
                json!({"model": "m", "messages": [{"role": "system", "content": [{"type": "document", "source": {}}]}]}),
                "a system message",
            ),
            (json!({"model": "m"}), "no messages"),
            (json!([]), "not a JSON object"),
        ] {
            let error = request_to_chat(&bad, "m").unwrap_err();
            assert!(error.contains(why), "{error}");
        }
        assert!(translatable("POST", "/v1/messages?beta=true"));
        assert!(!translatable("POST", "/v1/messages/count_tokens"));
        assert!(!translatable("GET", "/v1/messages"));
    }

    #[test]
    fn a_completion_becomes_a_message() {
        let response = json!({
            "id": "chatcmpl-1",
            "model": "gpt-5",
            "choices": [{"finish_reason": "tool_calls", "message": {
                "role": "assistant",
                "content": "Reading.",
                "reasoning_content": "Let me think.",
                "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "read", "arguments": "{\"path\": \"a.txt\"}"}},
                    {"id": "call_2", "type": "function", "function": {"name": "odd", "arguments": "not json"}}
                ]
            }}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 20, "prompt_tokens_details": {"cached_tokens": 60}}
        });
        let message = response_to_message(&response, "claude-sonnet-4-6").unwrap();
        assert_eq!(message["id"], "msg_chatcmpl-1");
        assert_eq!(message["type"], "message");
        assert_eq!(message["role"], "assistant");
        assert_eq!(message["model"], "claude-sonnet-4-6");
        assert_eq!(message["stop_reason"], "tool_use");
        assert_eq!(
            message["content"],
            json!([
                {"type": "thinking", "thinking": "Let me think."},
                {"type": "text", "text": "Reading."},
                {"type": "tool_use", "id": "call_1", "name": "read", "input": {"path": "a.txt"}},
                {"type": "tool_use", "id": "call_2", "name": "odd", "input": {"arguments": "not json"}}
            ])
        );
        assert_eq!(
            message["usage"],
            json!({"input_tokens": 40, "output_tokens": 20, "cache_read_input_tokens": 60, "cache_creation_input_tokens": 0})
        );
        let plain = json!({"choices": [{"finish_reason": "length", "message": {"content": "hi"}}]});
        let message = response_to_message(&plain, "m").unwrap();
        assert_eq!(message["stop_reason"], "max_tokens");
        assert_eq!(message["content"], json!([{"type": "text", "text": "hi"}]));
        assert_eq!(message["usage"]["input_tokens"], 0);
        assert!(message["id"].as_str().unwrap().starts_with("msg_"));
        assert_eq!(
            response_to_message(
                &json!({"choices": [{"finish_reason": "content_filter", "message": {}}]}),
                "m"
            )
            .unwrap()["stop_reason"],
            "refusal"
        );
        assert!(response_to_message(&json!({"choices": []}), "m").is_err());
        assert!(response_to_message(&json!({"error": "x"}), "m").is_err());

        let error = error_to_anthropic(
            429,
            br#"{"error": {"message": "slow down", "code": "rate_limit_exceeded"}}"#,
        );
        assert_eq!(
            error,
            json!({"type": "error", "error": {"type": "rate_limit_error", "message": "slow down"}})
        );
        assert_eq!(
            error_to_anthropic(502, b"")["error"]["message"],
            "the backend answered 502"
        );
        assert_eq!(
            error_to_anthropic(401, b"nope")["error"]["type"],
            "authentication_error"
        );
        assert_eq!(error_to_anthropic(401, b"nope")["error"]["message"], "nope");
    }

    #[test]
    fn a_chat_stream_becomes_anthropic_events_whatever_the_chunking() {
        let chunks = [
            json!({"id": "chatcmpl-9", "choices": [{"delta": {"role": "assistant", "content": ""}}]}),
            json!({"choices": [{"delta": {"content": "Hel"}}]}),
            json!({"choices": [{"delta": {"content": "lo"}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call_1", "function": {"name": "read", "arguments": ""}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "{\"pa"}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "th\":1}"}}]}}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 100, "completion_tokens": 20, "prompt_tokens_details": {"cached_tokens": 60}}}),
        ];
        let mut stream = String::new();
        for chunk in &chunks {
            stream.push_str(&format!("data: {chunk}\n\n"));
        }
        stream.push_str("data: [DONE]\n\n");
        let bytes = stream.as_bytes();
        let expected = vec![
            (
                "message_start",
                json!({"type": "message_start", "message": {
                "id": "msg_chatcmpl-9", "type": "message", "role": "assistant",
                "model": "claude-sonnet-4-6", "content": [], "stop_reason": null,
                "stop_sequence": null, "usage": {"input_tokens": 0, "output_tokens": 0}}}),
            ),
            (
                "content_block_start",
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            ),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hel"}}),
            ),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "lo"}}),
            ),
            (
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 0}),
            ),
            (
                "content_block_start",
                json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "call_1", "name": "read", "input": {}}}),
            ),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"pa"}}),
            ),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "th\":1}"}}),
            ),
            (
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 1}),
            ),
            (
                "message_delta",
                json!({"type": "message_delta", "delta": {"stop_reason": "tool_use", "stop_sequence": null},
                "usage": {"input_tokens": 40, "output_tokens": 20, "cache_read_input_tokens": 60, "cache_creation_input_tokens": 0}}),
            ),
            ("message_stop", json!({"type": "message_stop"})),
        ];
        let expected: Vec<(String, Value)> = expected
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();
        // Whole, byte by byte, and in uneven pieces: the same events.
        for step in [bytes.len(), 1, 7, 64] {
            let mut translator = StreamTranslator::new("claude-sonnet-4-6");
            let mut out = Vec::new();
            for piece in bytes.chunks(step) {
                out.extend(translator.feed(piece));
            }
            out.extend(translator.finish());
            assert_eq!(events(&out), expected, "step {step}");
        }
        // Without [DONE], the end of the stream ends the message; an error
        // chunk becomes an error event; reasoning becomes thinking.
        let mut translator = StreamTranslator::new("m");
        let mut out = translator.feed(
            b"data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"hm\"}}]}\n\n\
              data: {\"choices\":[{\"delta\":{\"content\":\"x\"},\"finish_reason\":\"stop\"}]}\n\n\
              data: {\"error\":{\"message\":\"boom\"}}\n\n",
        );
        out.extend(translator.finish());
        let got: Vec<_> = events(&out)
            .into_iter()
            .map(|(k, v)| {
                (
                    k,
                    v["content_block"]["type"]
                        .as_str()
                        .or_else(|| v["delta"]["type"].as_str())
                        .or_else(|| v["delta"]["stop_reason"].as_str())
                        .or_else(|| v["error"]["message"].as_str())
                        .unwrap_or("")
                        .to_owned(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("message_start".to_owned(), String::new()),
                ("content_block_start".to_owned(), "thinking".to_owned()),
                (
                    "content_block_delta".to_owned(),
                    "thinking_delta".to_owned()
                ),
                ("content_block_stop".to_owned(), String::new()),
                ("content_block_start".to_owned(), "text".to_owned()),
                ("content_block_delta".to_owned(), "text_delta".to_owned()),
                ("content_block_stop".to_owned(), String::new()),
                ("error".to_owned(), "boom".to_owned()),
                ("message_delta".to_owned(), "end_turn".to_owned()),
                ("message_stop".to_owned(), String::new()),
            ]
        );
        // Nothing in, nothing out.
        assert!(StreamTranslator::new("m").finish().is_empty());
    }
}
