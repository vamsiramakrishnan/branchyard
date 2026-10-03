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

/// Reads a response's usage as its bytes pass to the harness.
pub struct Meter {
    api: Api,
    sse: bool,
    body: Vec<u8>,
    overflow: bool,
    line: Vec<u8>,
    data: String,
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
            line: Vec::new(),
            data: String::new(),
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
        for &byte in bytes {
            if byte == b'\n' {
                let line = std::mem::take(&mut self.line);
                self.line_done(&line);
            } else {
                self.line.push(byte);
            }
        }
    }

    fn line_done(&mut self, line: &[u8]) {
        let line = String::from_utf8_lossy(line);
        let line = line.strip_suffix('\r').unwrap_or(&line);
        if line.is_empty() {
            let data = std::mem::take(&mut self.data);
            self.event(&data);
            return;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            if !self.data.is_empty() {
                self.data.push('\n');
            }
            self.data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
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
            if !self.line.is_empty() {
                let line = std::mem::take(&mut self.line);
                self.line_done(&line);
            }
            if !self.data.is_empty() {
                let data = std::mem::take(&mut self.data);
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
}
