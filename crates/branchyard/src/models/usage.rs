//! A call's tokens, from the provider's own usage fields: in the JSON
//! body, or in a stream's events as they pass.
//!
//! - Anthropic: `usage` (`input_tokens`, `output_tokens`,
//!   `cache_read_input_tokens`, `cache_creation_input_tokens`, and
//!   `cache_creation.ephemeral_1h_input_tokens`); streamed, `message_start`
//!   carries the input side and each `message_delta` the output so far.
//! - OpenAI chat completions: `usage` (`prompt_tokens`, which include
//!   `prompt_tokens_details.cached_tokens`, and `completion_tokens`);
//!   streamed, the last chunk's `usage`, which the gateway asks for with
//!   `stream_options.include_usage`.
//! - OpenAI responses: `usage` (`input_tokens`, which include
//!   `input_tokens_details.cached_tokens`, and `output_tokens`); streamed,
//!   `response.completed`'s `response.usage`.
//!
//! [`Tokens::input`] is always the uncached input: a cached token is
//! counted once, as a cache read.
//!
//! Beside the [`Meter`], a [`Collector`] reads what the response did (the
//! tools it called, why it stopped, how many bytes it was) and [`Asked`]
//! what the request offered; together they make a call's
//! [`Interception`](super::Interception). Both are read from the
//! backend's own shape, whatever API the harness spoke: a call translated
//! for an OpenAI backend is read from the chat completion the backend sent.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Api;

/// A call's tokens.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    /// Input not read from a cache.
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    /// Written to a cache that lives five minutes (Anthropic's default),
    /// or the provider's only cache.
    pub cache_write: u64,
    /// Written to Anthropic's one-hour cache.
    #[serde(default)]
    pub cache_write_1h: u64,
}

impl Tokens {
    /// Every input token, cached or not.
    pub fn input_total(&self) -> u64 {
        self.input + self.cache_read + self.cache_write + self.cache_write_1h
    }

    /// Every token.
    pub fn total(&self) -> u64 {
        self.input_total() + self.output
    }
}

fn number(value: &Value, path: &[&str]) -> Option<u64> {
    let mut at = value;
    for key in path {
        at = at.get(key)?;
    }
    at.as_u64()
}

/// Tokens from a `usage` object of `api`'s shape; `None` when it has none
/// of the fields.
pub fn from_usage(api: Api, usage: &Value) -> Option<Tokens> {
    if !usage.is_object() {
        return None;
    }
    let get = |path: &[&str]| number(usage, path);
    // OpenAI's chat completions.
    if let Some(prompt) = get(&["prompt_tokens"]) {
        let cached = get(&["prompt_tokens_details", "cached_tokens"]).unwrap_or(0);
        return Some(Tokens {
            input: prompt.saturating_sub(cached),
            output: get(&["completion_tokens"]).unwrap_or(0),
            cache_read: cached.min(prompt),
            ..Tokens::default()
        });
    }
    let input = get(&["input_tokens"]);
    let output = get(&["output_tokens"]);
    if input.is_none() && output.is_none() {
        return None;
    }
    let openai_responses = api == Api::Openai
        || usage.get("input_tokens_details").is_some()
        || usage.get("output_tokens_details").is_some();
    if openai_responses {
        let input = input.unwrap_or(0);
        let cached = get(&["input_tokens_details", "cached_tokens"]).unwrap_or(0);
        return Some(Tokens {
            input: input.saturating_sub(cached),
            output: output.unwrap_or(0),
            cache_read: cached.min(input),
            ..Tokens::default()
        });
    }
    let created = get(&["cache_creation_input_tokens"]).unwrap_or(0);
    let one_hour = get(&["cache_creation", "ephemeral_1h_input_tokens"])
        .unwrap_or(0)
        .min(created);
    Some(Tokens {
        input: input.unwrap_or(0),
        output: output.unwrap_or(0),
        cache_read: get(&["cache_read_input_tokens"]).unwrap_or(0),
        cache_write: created - one_hour,
        cache_write_1h: one_hour,
    })
}

/// The largest body kept to read a non-streamed response's usage from.
const MAX_BODY: usize = 16 * 1024 * 1024;

/// Server-sent events from bytes in any chunking: a line ends at `\n` (a
/// `\r` before it is dropped), a `data:` line adds to the event, and a
/// blank line ends it. The one reader the [`Meter`] and the [`Collector`]
/// share.
#[derive(Default)]
struct Events {
    line: Vec<u8>,
    data: String,
}

impl Events {
    /// More bytes of the stream: the data of each event they completed.
    fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut done = Vec::new();
        for &byte in bytes {
            if byte == b'\n' {
                let line = std::mem::take(&mut self.line);
                self.line_done(&line, &mut done);
            } else {
                self.line.push(byte);
            }
        }
        done
    }

    fn line_done(&mut self, line: &[u8], done: &mut Vec<String>) {
        let line = String::from_utf8_lossy(line);
        let line = line.strip_suffix('\r').unwrap_or(&line);
        if line.is_empty() {
            if !self.data.is_empty() {
                done.push(std::mem::take(&mut self.data));
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

    /// The stream ended: a last line without its newline and a last event
    /// without its blank line count.
    fn finish(&mut self) -> Vec<String> {
        let mut done = Vec::new();
        if !self.line.is_empty() {
            let line = std::mem::take(&mut self.line);
            self.line_done(&line, &mut done);
        }
        if !self.data.is_empty() {
            done.push(std::mem::take(&mut self.data));
        }
        done
    }
}

/// Reads a response's usage as its bytes pass to the harness.
pub struct Meter {
    api: Api,
    sse: bool,
    body: Vec<u8>,
    overflow: bool,
    events: Events,
    tokens: Option<Tokens>,
    model: Option<String>,
}

impl Meter {
    pub fn new(api: Api, sse: bool) -> Meter {
        Meter {
            api,
            sse,
            body: Vec::new(),
            overflow: false,
            events: Events::default(),
            tokens: None,
            model: None,
        }
    }

    /// Bytes of the response body, in order.
    pub fn feed(&mut self, bytes: &[u8]) {
        if !self.sse {
            if self.body.len() + bytes.len() > MAX_BODY {
                self.overflow = true;
            } else {
                self.body.extend_from_slice(bytes);
            }
            return;
        }
        for data in self.events.feed(bytes) {
            self.event(&data);
        }
    }

    /// One server-sent event's data.
    fn event(&mut self, data: &str) {
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            return;
        };
        match value.get("type").and_then(Value::as_str) {
            // Anthropic: the input side, and the output so far.
            Some("message_start") => {
                let message = &value["message"];
                self.note_model(message);
                if let Some(tokens) = from_usage(Api::Anthropic, &message["usage"]) {
                    self.tokens = Some(tokens);
                }
            }
            Some("message_delta") => {
                let usage = &value["usage"];
                let mut tokens = self.tokens.unwrap_or_default();
                if let Some(output) = number(usage, &["output_tokens"]) {
                    tokens.output = output;
                }
                // Newer streams repeat the input side here; take it when
                // they do.
                if let Some(full) = from_usage(Api::Anthropic, usage) {
                    if usage.get("input_tokens").is_some() {
                        tokens.input = full.input;
                    }
                    if usage.get("cache_read_input_tokens").is_some() {
                        tokens.cache_read = full.cache_read;
                    }
                    if usage.get("cache_creation_input_tokens").is_some() {
                        tokens.cache_write = full.cache_write;
                        tokens.cache_write_1h = full.cache_write_1h;
                    }
                }
                self.tokens = Some(tokens);
            }
            // OpenAI responses: the final response.
            Some("response.completed") | Some("response.incomplete") | Some("response.failed") => {
                let response = &value["response"];
                self.note_model(response);
                if let Some(tokens) = from_usage(Api::Openai, &response["usage"]) {
                    self.tokens = Some(tokens);
                }
            }
            // OpenAI chat completions: the chunk that carries usage.
            _ => {
                self.note_model(&value);
                if let Some(tokens) = value.get("usage").and_then(|u| from_usage(self.api, u)) {
                    self.tokens = Some(tokens);
                }
            }
        }
    }

    fn note_model(&mut self, value: &Value) {
        if let Some(model) = value.get("model").and_then(Value::as_str) {
            self.model = Some(model.to_owned());
        }
    }

    /// The call's tokens and the model the provider says answered.
    pub fn finish(mut self) -> (Option<Tokens>, Option<String>) {
        if self.sse {
            for data in self.events.finish() {
                self.event(&data);
            }
            return (self.tokens, self.model);
        }
        if self.overflow {
            return (None, None);
        }
        let Ok(value) = serde_json::from_slice::<Value>(&self.body) else {
            return (None, None);
        };
        // A responses object nests nothing; a chat completion and a
        // message carry `usage` at the top.
        let tokens = value.get("usage").and_then(|u| from_usage(self.api, u));
        let model = value
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_owned);
        (tokens, model)
    }
}

/// What a request asked for, read from the body the harness sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Asked {
    /// Bytes of the body.
    pub bytes: u64,
    /// Lowercase hex of the BLAKE3 hash of the body's bytes.
    pub digest: String,
    /// `messages` (Anthropic and OpenAI chat) or `input` items (OpenAI
    /// responses) in the body.
    pub messages: u32,
    /// The names in the body's `tools`.
    pub tools_offered: Vec<String>,
}

impl Asked {
    /// `body` is exactly what the harness sent, with HTTP framing (a
    /// chunked coding) removed and nothing the gateway adds; `json` is its
    /// parse when it is JSON.
    pub(crate) fn new(body: &[u8], json: Option<&Value>) -> Asked {
        Asked {
            bytes: body.len() as u64,
            digest: blake3::hash(body).to_hex().to_string(),
            messages: json.map_or(0, messages),
            tools_offered: json.map_or_else(Vec::new, offered),
        }
    }
}

/// A non-empty string.
fn text(value: &Value) -> Option<String> {
    value.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

/// The items of a request: its `messages`, else its `input` (a string is
/// one item).
fn messages(request: &Value) -> u32 {
    let count = match (&request["messages"], &request["input"]) {
        (Value::Array(items), _) | (_, Value::Array(items)) => items.len(),
        (_, Value::String(_)) => 1,
        _ => 0,
    };
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// The names of a request's `tools`, in order: Anthropic's `name`, OpenAI
/// chat's `function.name`, the responses API's `name`; a tool with none
/// (OpenAI's built-in `web_search_preview`) is named by its `type`.
fn offered(request: &Value) -> Vec<String> {
    let Some(tools) = request["tools"].as_array() else {
        return Vec::new();
    };
    tools
        .iter()
        .filter_map(|tool| {
            text(&tool["name"])
                .or_else(|| text(&tool["function"]["name"]))
                .or_else(|| text(&tool["type"]))
        })
        .collect()
}

/// What a response did, besides its tokens.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Observed {
    /// Bytes of the response body.
    pub bytes: u64,
    /// The tools or functions it called, in order.
    pub tool_uses: Vec<String>,
    /// Why it stopped, in the provider's words.
    pub stop_reason: Option<String>,
}

/// Reads which tools a response called, and why it stopped, as its bytes
/// pass to the harness: from the JSON body, or from a stream's events.
///
/// The shape is the response's own, not the request's, so a call
/// translated for another API is read from what the backend sent:
///
/// - Anthropic: `content[]` blocks of type `tool_use` and `stop_reason`;
///   streamed, `content_block_start` of a `tool_use` block, and
///   `message_delta`'s `delta.stop_reason`.
/// - OpenAI chat completions: `choices[].message.tool_calls[]` and the first
///   choice's `finish_reason`; streamed, `choices[].delta.tool_calls[]`, a
///   call's name arriving with its first delta, and the last `finish_reason`.
/// - OpenAI responses: `output[]` items of type `function_call` (or
///   `custom_tool_call`) and `status`; streamed, `response.output_item.done`
///   and the final `response.completed`'s `status`.
pub(crate) struct Collector {
    sse: bool,
    bytes: u64,
    body: Vec<u8>,
    overflow: bool,
    events: Events,
    tool_uses: Vec<String>,
    stop_reason: Option<String>,
    /// Chat stream tool calls already named, by choice and call index.
    named: BTreeSet<(u64, u64)>,
    /// The calls in a responses stream's final response: used only when
    /// no output item of the stream completed with one.
    completed: Vec<String>,
}

impl Collector {
    /// A collector for a stream of server-sent events (`sse`) or a JSON
    /// body.
    pub(crate) fn new(sse: bool) -> Collector {
        Collector {
            sse,
            bytes: 0,
            body: Vec::new(),
            overflow: false,
            events: Events::default(),
            tool_uses: Vec::new(),
            stop_reason: None,
            named: BTreeSet::new(),
            completed: Vec::new(),
        }
    }

    /// Bytes of the response body, in order.
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        self.bytes = self.bytes.saturating_add(bytes.len() as u64);
        if !self.sse {
            if self.body.len() + bytes.len() > MAX_BODY {
                self.overflow = true;
            } else {
                self.body.extend_from_slice(bytes);
            }
            return;
        }
        for data in self.events.feed(bytes) {
            self.event(&data);
        }
    }

    fn event(&mut self, data: &str) {
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            return;
        };
        match value.get("type").and_then(Value::as_str) {
            // Anthropic: a tool block opens with its name; the stop reason
            // arrives in the last message delta.
            Some("content_block_start") => {
                let block = &value["content_block"];
                if block["type"] == "tool_use" {
                    self.tool_uses.extend(text(&block["name"]));
                }
            }
            Some("message_start") => self.stop(&value["message"]["stop_reason"]),
            Some("message_delta") => self.stop(&value["delta"]["stop_reason"]),
            // OpenAI responses: an output item is complete with its name.
            Some("response.output_item.done") => {
                self.tool_uses.extend(called(&value["item"]));
            }
            Some("response.completed") | Some("response.incomplete") | Some("response.failed") => {
                let response = &value["response"];
                self.stop(&response["status"]);
                self.completed = from_body(response).0;
            }
            // OpenAI chat completions: a chunk.
            _ => self.chunk(&value),
        }
    }

    fn chunk(&mut self, chunk: &Value) {
        let Some(choices) = chunk["choices"].as_array() else {
            return;
        };
        for choice in choices {
            let at = choice["index"].as_u64().unwrap_or(0);
            if let Some(calls) = choice["delta"]["tool_calls"].as_array() {
                for call in calls {
                    let Some(name) = text(&call["function"]["name"]) else {
                        continue;
                    };
                    // A call's name comes with its first delta; a server
                    // that repeats it is not counted twice.
                    let new = match call["index"].as_u64() {
                        Some(index) => self.named.insert((at, index)),
                        None => true,
                    };
                    if new {
                        self.tool_uses.push(name);
                    }
                }
            }
            if at == 0 {
                self.stop(&choice["finish_reason"]);
            }
        }
    }

    /// The latest stop reason the response gave.
    fn stop(&mut self, reason: &Value) {
        if let Some(reason) = text(reason) {
            self.stop_reason = Some(reason);
        }
    }

    /// What the response did. A body that is not JSON, or too large to
    /// have been kept, calls no tools and gives no reason.
    pub(crate) fn finish(mut self) -> Observed {
        if self.sse {
            for data in self.events.finish() {
                self.event(&data);
            }
            if self.tool_uses.is_empty() {
                self.tool_uses = std::mem::take(&mut self.completed);
            }
        } else if !self.overflow {
            if let Ok(value) = serde_json::from_slice::<Value>(&self.body) {
                (self.tool_uses, self.stop_reason) = from_body(&value);
            }
        }
        Observed {
            bytes: self.bytes,
            tool_uses: self.tool_uses,
            stop_reason: self.stop_reason,
        }
    }
}

/// The name of a responses `output[]` item that is a call.
fn called(item: &Value) -> Option<String> {
    match item["type"].as_str() {
        Some("function_call") | Some("custom_tool_call") => text(&item["name"]),
        _ => None,
    }
}

/// The tools a whole response called, in order, and why it stopped.
fn from_body(response: &Value) -> (Vec<String>, Option<String>) {
    let mut tools = Vec::new();
    // Anthropic's message.
    if let Some(blocks) = response["content"].as_array() {
        for block in blocks.iter().filter(|b| b["type"] == "tool_use") {
            tools.extend(text(&block["name"]));
        }
    }
    // OpenAI's chat completion.
    if let Some(choices) = response["choices"].as_array() {
        for choice in choices {
            if let Some(calls) = choice["message"]["tool_calls"].as_array() {
                tools.extend(calls.iter().filter_map(|c| text(&c["function"]["name"])));
            }
        }
    }
    // OpenAI's response object.
    let responses = response["output"].as_array();
    if let Some(items) = responses {
        tools.extend(items.iter().filter_map(called));
    }
    let stop = text(&response["stop_reason"])
        .or_else(|| text(&response["choices"][0]["finish_reason"]))
        .or_else(|| responses.and_then(|_| text(&response["status"])));
    (tools, stop)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(api: Api, sse: bool, chunks: &[&str]) -> (Option<Tokens>, Option<String>) {
        let mut meter = Meter::new(api, sse);
        for chunk in chunks {
            meter.feed(chunk.as_bytes());
        }
        meter.finish()
    }

    #[test]
    fn anthropic_usage_counts_each_token_once() {
        let body = r#"{"model":"claude-sonnet-4-6","usage":{"input_tokens":10,"output_tokens":5,
            "cache_read_input_tokens":100,"cache_creation_input_tokens":40,
            "cache_creation":{"ephemeral_5m_input_tokens":30,"ephemeral_1h_input_tokens":10}}}"#;
        let (tokens, model) = feed(Api::Anthropic, false, &[&body[..20], &body[20..]]);
        assert_eq!(
            tokens,
            Some(Tokens {
                input: 10,
                output: 5,
                cache_read: 100,
                cache_write: 30,
                cache_write_1h: 10
            })
        );
        assert_eq!(model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(tokens.unwrap().total(), 155);
    }

    #[test]
    fn a_stream_is_read_across_chunk_boundaries() {
        let stream = "event: message_start\r\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-haiku-4-5\",\"usage\":{\"input_tokens\":7,\"output_tokens\":1,\"cache_read_input_tokens\":3}}}\r\n\r\n\
            event: content_block_delta\ndata: {\"type\":\"content_block_delta\"}\n\n\
            event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}\n\n\
            event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        for cut in [1, 17, 64, 200, stream.len() - 3] {
            let (tokens, model) = feed(Api::Anthropic, true, &[&stream[..cut], &stream[cut..]]);
            assert_eq!(
                tokens,
                Some(Tokens {
                    input: 7,
                    output: 9,
                    cache_read: 3,
                    ..Tokens::default()
                }),
                "cut at {cut}"
            );
            assert_eq!(model.as_deref(), Some("claude-haiku-4-5"));
        }
    }

    #[test]
    fn openai_chat_and_responses_usage_split_out_cached_input() {
        let chat = r#"{"model":"gpt-5","usage":{"prompt_tokens":100,"completion_tokens":20,
            "prompt_tokens_details":{"cached_tokens":60}}}"#;
        let (tokens, _) = feed(Api::Openai, false, &[chat]);
        assert_eq!(
            tokens,
            Some(Tokens {
                input: 40,
                output: 20,
                cache_read: 60,
                ..Tokens::default()
            })
        );
        let stream = "data: {\"model\":\"gpt-5\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}],\"usage\":null}\n\n\
            data: {\"model\":\"gpt-5\",\"choices\":[],\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":2}}\n\n\
            data: [DONE]\n\n";
        let (tokens, model) = feed(Api::Openai, true, &[stream]);
        assert_eq!(tokens.map(|t| (t.input, t.output)), Some((8, 2)));
        assert_eq!(model.as_deref(), Some("gpt-5"));
        let responses = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"model\":\"gpt-5\"}}\n\n\
            event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"model\":\"gpt-5\",\"usage\":{\"input_tokens\":30,\"output_tokens\":4,\"input_tokens_details\":{\"cached_tokens\":10}}}}\n\n";
        let (tokens, _) = feed(Api::Openai, true, &[responses]);
        assert_eq!(
            tokens,
            Some(Tokens {
                input: 20,
                output: 4,
                cache_read: 10,
                ..Tokens::default()
            })
        );
        // A body that is not JSON, or has no usage, meters nothing.
        assert_eq!(feed(Api::Openai, false, &["not json"]).0, None);
        assert_eq!(feed(Api::Openai, false, &["{}"]).0, None);
    }

    fn observe(sse: bool, chunks: &[&[u8]]) -> Observed {
        let mut collector = Collector::new(sse);
        for chunk in chunks {
            collector.feed(chunk);
        }
        collector.finish()
    }

    /// What a collector reads from `text`: the same whole, cut in two at
    /// every byte (inside a multi-byte character too), and a byte at a
    /// time.
    fn collect(sse: bool, text: &str) -> Observed {
        let whole = observe(sse, &[text.as_bytes()]);
        assert_eq!(whole.bytes, text.len() as u64);
        for cut in 0..=text.len() {
            let (head, tail) = text.as_bytes().split_at(cut);
            assert_eq!(observe(sse, &[head, tail]), whole, "cut at {cut}");
        }
        let singly: Vec<&[u8]> = text.as_bytes().chunks(1).collect();
        assert_eq!(observe(sse, &singly), whole, "a byte at a time");
        whole
    }

    #[test]
    fn an_anthropic_message_names_its_tool_uses_in_order() {
        let body = r#"{"type":"message","model":"claude-sonnet-4-6","content":[
            {"type":"text","text":"I will look"},
            {"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"/a"}},
            {"type":"tool_use","id":"toolu_2","name":"Edit","input":{}},
            {"type":"tool_use","id":"toolu_3","name":"Read","input":{}}],
            "stop_reason":"tool_use","usage":{"input_tokens":1,"output_tokens":2}}"#;
        let seen = collect(false, body);
        assert_eq!(seen.tool_uses, ["Read", "Edit", "Read"]);
        assert_eq!(seen.stop_reason.as_deref(), Some("tool_use"));
        // Text alone calls nothing.
        let text = r#"{"type":"message","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}"#;
        let seen = collect(false, text);
        assert!(seen.tool_uses.is_empty());
        assert_eq!(seen.stop_reason.as_deref(), Some("end_turn"));
    }

    #[test]
    fn an_anthropic_stream_names_each_tool_block_as_it_opens() {
        let stream = "event: message_start\r\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-haiku-4-5\",\"stop_reason\":null,\"usage\":{\"input_tokens\":7,\"output_tokens\":1}}}\r\n\r\n\
            event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
            event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"caf\u{e9} \u{2615}\"}}\n\n\
            event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"Bash\",\"input\":{}}}\n\n\
            event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"name\\\":\\\"Nope\\\"}\"}}\n\n\
            event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n\
            event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_2\",\"name\":\"Read\",\"input\":{}}}\n\n\
            event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":9}}\n\n\
            event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let seen = collect(true, stream);
        assert_eq!(seen.tool_uses, ["Bash", "Read"]);
        assert_eq!(seen.stop_reason.as_deref(), Some("tool_use"));
        // A stream that ends without its last blank line still counts it.
        let cut = stream.trim_end_matches('\n');
        let seen = collect(true, cut);
        assert_eq!(seen.stop_reason.as_deref(), Some("tool_use"));
        // A stream that stops before any reason gives none.
        let head = &stream[..stream.find("event: message_delta").unwrap()];
        let seen = collect(true, head);
        assert_eq!(seen.tool_uses, ["Bash", "Read"]);
        assert_eq!(seen.stop_reason, None);
    }

    #[test]
    fn an_openai_chat_completion_names_its_tool_calls() {
        let body = r#"{"id":"c1","object":"chat.completion","model":"gpt-5","choices":[
            {"index":0,"message":{"role":"assistant","content":null,"tool_calls":[
                {"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{}"}},
                {"id":"call_2","type":"function","function":{"name":"write_file","arguments":"{}"}}]},
             "finish_reason":"tool_calls"}],
            "usage":{"prompt_tokens":8,"completion_tokens":2}}"#;
        let seen = collect(false, body);
        assert_eq!(seen.tool_uses, ["read_file", "write_file"]);
        assert_eq!(seen.stop_reason.as_deref(), Some("tool_calls"));
        let text = r#"{"choices":[{"message":{"content":"hi"},"finish_reason":"stop"}]}"#;
        let seen = collect(false, text);
        assert!(seen.tool_uses.is_empty());
        assert_eq!(seen.stop_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn an_openai_chat_stream_names_a_call_with_its_first_delta() {
        let stream = "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":null,\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n\
            data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\"}}]},\"finish_reason\":null}]}\n\n\
            data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"a\\\"}\"}}]},\"finish_reason\":null}]}\n\n\
            data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_2\",\"type\":\"function\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n\
            data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
            data: {\"choices\":[],\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":2}}\n\n\
            data: [DONE]\n\n";
        let seen = collect(true, stream);
        assert_eq!(seen.tool_uses, ["read_file", "write_file"]);
        assert_eq!(seen.stop_reason.as_deref(), Some("tool_calls"));
        // A server that repeats a call's name in every delta, with its
        // index, is not counted twice.
        let repeated = "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\"}}]}}]}\n\n\
            data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"read_file\",\"arguments\":\"}\"}}]},\"finish_reason\":\"stop\"}]}\n\n";
        let seen = collect(true, repeated);
        assert_eq!(seen.tool_uses, ["read_file"]);
        assert_eq!(seen.stop_reason.as_deref(), Some("stop"));
        // Two choices keep their calls apart; the reason is the first
        // choice's.
        let two = "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"a\"}}]}},{\"index\":1,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"b\"}}]}}]}\n\n\
            data: {\"choices\":[{\"index\":1,\"delta\":{},\"finish_reason\":\"length\"},{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n";
        let seen = collect(true, two);
        assert_eq!(seen.tool_uses, ["a", "b"]);
        assert_eq!(seen.stop_reason.as_deref(), Some("tool_calls"));
        // Plain text calls nothing.
        let text = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n\
            data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
            data: [DONE]\n\n";
        let seen = collect(true, text);
        assert!(seen.tool_uses.is_empty());
        assert_eq!(seen.stop_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn an_openai_response_names_its_function_calls() {
        let body = r#"{"id":"resp_1","object":"response","status":"completed","output":[
            {"type":"reasoning","summary":[]},
            {"type":"function_call","call_id":"c1","name":"shell","arguments":"{}"},
            {"type":"message","role":"assistant","content":[]},
            {"type":"custom_tool_call","call_id":"c2","name":"apply_patch","input":"*** Begin Patch"}],
            "usage":{"input_tokens":3,"output_tokens":4}}"#;
        let seen = collect(false, body);
        assert_eq!(seen.tool_uses, ["shell", "apply_patch"]);
        assert_eq!(seen.stop_reason.as_deref(), Some("completed"));
        // A `status` without `output` is not a response's.
        let other = r#"{"status":"ok","data":[]}"#;
        assert_eq!(collect(false, other).stop_reason, None);
    }

    #[test]
    fn an_openai_responses_stream_names_each_call_when_its_item_is_done() {
        let item = |kind: &str, name: &str| {
            format!(r#"{{"type":"{kind}","call_id":"c","name":"{name}","arguments":"{{}}"}}"#)
        };
        let stream = format!(
            "event: response.created\ndata: {{\"type\":\"response.created\",\"response\":{{\"status\":\"in_progress\"}}}}\n\n\
             event: response.output_item.added\ndata: {{\"type\":\"response.output_item.added\",\"item\":{}}}\n\n\
             event: response.function_call_arguments.delta\ndata: {{\"type\":\"response.function_call_arguments.delta\",\"delta\":\"{{\"}}\n\n\
             event: response.output_item.done\ndata: {{\"type\":\"response.output_item.done\",\"item\":{}}}\n\n\
             event: response.output_item.done\ndata: {{\"type\":\"response.output_item.done\",\"item\":{{\"type\":\"message\",\"content\":[]}}}}\n\n\
             event: response.output_item.done\ndata: {{\"type\":\"response.output_item.done\",\"item\":{}}}\n\n\
             event: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"status\":\"completed\",\"output\":[{},{}],\"usage\":{{\"input_tokens\":3,\"output_tokens\":4}}}}}}\n\n",
            item("function_call", "shell"),
            item("function_call", "shell"),
            item("function_call", "update_plan"),
            item("function_call", "shell"),
            item("function_call", "update_plan"),
        );
        // Counted once, from the done items, not again from the final
        // response that lists them.
        let seen = collect(true, &stream);
        assert_eq!(seen.tool_uses, ["shell", "update_plan"]);
        assert_eq!(seen.stop_reason.as_deref(), Some("completed"));
        // A stream with only the final response falls back to its output.
        let only = format!(
            "event: response.incomplete\ndata: {{\"type\":\"response.incomplete\",\"response\":{{\"status\":\"incomplete\",\"output\":[{}]}}}}\n\n",
            item("function_call", "shell"),
        );
        let seen = collect(true, &only);
        assert_eq!(seen.tool_uses, ["shell"]);
        assert_eq!(seen.stop_reason.as_deref(), Some("incomplete"));
    }

    #[test]
    fn a_response_that_calls_nothing_reads_as_nothing_and_still_counts_its_bytes() {
        for (sse, body) in [
            (false, ""),
            (false, "not json"),
            (
                false,
                r#"{"error":{"type":"overloaded_error","message":"busy"}}"#,
            ),
            (false, r#"{"content":"a string","choices":"no","output":3}"#),
            (true, ""),
            (true, "data: not json\n\ndata: [DONE]\n\n"),
            (true, ": a comment\nevent: ping\n\n"),
        ] {
            let seen = collect(sse, body);
            assert_eq!(
                (seen.tool_uses.is_empty(), seen.stop_reason),
                (true, None),
                "{body:?}"
            );
        }
        // A name that is not a string, or is empty, is no tool.
        let odd = r#"{"content":[{"type":"tool_use","name":7},{"type":"tool_use","name":""},{"type":"tool_use"}]}"#;
        assert!(collect(false, odd).tool_uses.is_empty());
    }

    #[test]
    fn a_body_too_large_to_keep_is_counted_but_not_read() {
        let mut collector = Collector::new(false);
        let big = vec![b' '; MAX_BODY / 2 + 1];
        collector.feed(&big);
        collector.feed(&big);
        let call = br#"{"content":[{"type":"tool_use","name":"Read"}]}"#;
        collector.feed(call);
        let seen = collector.finish();
        assert_eq!(seen.bytes, 2 * big.len() as u64 + call.len() as u64);
        assert!(seen.tool_uses.is_empty());
    }

    #[test]
    fn a_request_is_counted_and_its_tools_named_in_each_api() {
        let ask = |request: serde_json::Value| {
            let body = request.to_string();
            Asked::new(body.as_bytes(), Some(&request))
        };
        let anthropic = ask(serde_json::json!({
            "model": "claude-sonnet-4-6", "max_tokens": 16,
            "messages": [{"role": "user", "content": "a"}, {"role": "assistant", "content": "b"},
                         {"role": "user", "content": "c"}],
            "tools": [{"name": "Read", "input_schema": {}}, {"name": "Edit", "input_schema": {}},
                      {"type": "web_search_20250305", "name": "web_search"}]
        }));
        assert_eq!(anthropic.messages, 3);
        assert_eq!(anthropic.tools_offered, ["Read", "Edit", "web_search"]);
        let chat = ask(serde_json::json!({
            "model": "gpt-5", "messages": [{"role": "system", "content": "s"}, {"role": "user", "content": "u"}],
            "tools": [{"type": "function", "function": {"name": "read_file", "parameters": {}}}]
        }));
        assert_eq!(chat.messages, 2);
        assert_eq!(chat.tools_offered, ["read_file"]);
        let responses = ask(serde_json::json!({
            "model": "gpt-5", "input": [{"type": "message"}, {"type": "function_call_output"},
                                        {"type": "message"}, {"type": "message"}],
            "tools": [{"type": "function", "name": "shell", "parameters": {}}, {"type": "web_search_preview"}]
        }));
        assert_eq!(responses.messages, 4);
        assert_eq!(responses.tools_offered, ["shell", "web_search_preview"]);
        // A string input is one item; a body without either is none.
        let text = ask(serde_json::json!({"model": "gpt-5", "input": "hi"}));
        assert_eq!((text.messages, text.tools_offered.len()), (1, 0));
        let bare = ask(serde_json::json!({"model": "gpt-5"}));
        assert_eq!((bare.messages, bare.tools_offered.len()), (0, 0));
        // A body that is not JSON has a size and a digest and nothing else.
        let raw = Asked::new(b"\x00\x01\x02", None);
        assert_eq!(
            (raw.bytes, raw.messages, raw.tools_offered.len()),
            (3, 0, 0)
        );
    }

    #[test]
    fn the_prompt_digest_is_the_blake3_hash_of_the_bytes_sent() {
        let sent = br#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#;
        let json: Value = serde_json::from_slice(sent).unwrap();
        let first = Asked::new(sent, Some(&json));
        assert_eq!(first.digest, blake3::hash(sent).to_hex().to_string());
        assert_eq!(first.digest.len(), 64);
        assert!(first
            .digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        assert_eq!(first.bytes, sent.len() as u64);
        // The same request gives the same digest, however often it is read.
        assert_eq!(Asked::new(sent, Some(&json)).digest, first.digest);
        // The bytes are hashed, not the JSON they mean: the same request
        // with other spacing, or any other request, differs.
        let spaced = br#"{ "model": "m", "messages": [{"role":"user","content":"hi"}] }"#;
        let again: Value = serde_json::from_slice(spaced).unwrap();
        assert_eq!(again, json);
        assert_ne!(Asked::new(spaced, Some(&again)).digest, first.digest);
        let other = br#"{"model":"m","messages":[{"role":"user","content":"ho"}]}"#;
        assert_ne!(Asked::new(other, None).digest, first.digest);
        // No body is the hash of the empty input (BLAKE3's published
        // test vector).
        assert_eq!(
            Asked::new(b"", None).digest,
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }
}
