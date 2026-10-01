// Derived from stablyai/orca at revision
// 280733273545f0b3eeedc1be54b14d406239030e:
// src/main/claude-usage/transcript-record-parser.ts,
// src/main/claude-usage/transcript-file-discovery.ts,
// src/main/claude-usage/claude-model-pricing.ts,
// src/main/codex-usage/codex-rollout-file-parse.ts,
// src/main/codex-usage/codex-usage-record-parser.ts,
// src/main/codex-usage/codex-model-pricing.ts,
// src/main/rate-limits/codex-rate-limit-window-classification.ts and
// src/main/rate-limits/codex-rate-limit-window-mapper.ts.
// Copyright (c) 2026 Lovecast Inc. Licensed under the MIT License; the
// license text, which must accompany substantial portions of this code, is
// in vendor/orca/LICENSE.
// Modified for Branchyard: translated from TypeScript to Rust as one
// read-only pass over the local transcripts (no store, no incremental
// resume, no worktree attribution, no OAuth, PTY or app-server probes,
// which need credentials); Orca's pricing tables are kept as data in
// catalog/pricing.toml and its model-name normalization is reduced to the
// matching rules below; Claude's 5-hour window is reconstructed from the
// transcripts' times (Claude Code records no limit on disk), and a
// "usage limit reached" message marks a window full.

//! `by usage`: per local login (Claude Code, Codex), how much of its
//! 5-hour and weekly windows is used and when each resets, read from the
//! harness's own session files, never from a credential. Codex records its
//! rate limits in each rollout's `token_count` events; Claude Code records
//! only token counts, so its windows are rebuilt from them, with a percent
//! only when `[usage]` says what a window allows. `by run` and `by fan`
//! warn (or refuse) near a limit, and the router can skip such a
//! candidate. See `docs/usage.md`.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use branchyard_setup::config::{UsageConfig, UsageGuard};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::render::{self, Style, Tone};

const HOUR_MS: u64 = 3_600_000;
/// Claude's and Codex's short window, and Orca's `CODEX_SESSION_WINDOW_MINUTES`.
pub const FIVE_HOUR_MINUTES: u32 = 300;
/// Orca's `CODEX_WEEKLY_WINDOW_MINUTES`.
pub const WEEKLY_MINUTES: u32 = 10_080;
const WEEK_MS: u64 = WEEKLY_MINUTES as u64 * 60_000;
const FIVE_HOURS_MS: u64 = FIVE_HOUR_MINUTES as u64 * 60_000;
/// Orca's tolerance for Codex windows reported a minute off.
const WINDOW_TOLERANCE_MINUTES: f64 = 1.0;
/// Default for `[usage] near_percent`.
pub const DEFAULT_NEAR_PERCENT: f64 = 90.0;
/// Files not modified for longer than this cannot hold anything in a
/// weekly window, and are not read.
const SCAN_HORIZON_MS: u64 = WEEK_MS + 2 * 86_400_000;

// Pricing, kept as data.

const PRICING_TOML: &str = include_str!("../../../catalog/pricing.toml");

/// Orca's `ClaudeModelPricing`, dollars per million tokens.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudePrice {
    pub model: String,
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub cache_write_1h: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold_tokens: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_above: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_above: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_above: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_above: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_1h_above: Option<f64>,
}

/// Orca's `CodexTokenRates`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodexRates {
    pub input: f64,
    pub cached_input: f64,
    pub output: f64,
}

/// Orca's `CodexModelPricing`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodexPrice {
    pub model: String,
    pub input: f64,
    pub cached_input: f64,
    pub output: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_context: Option<CodexRates>,
}

/// `catalog/pricing.toml`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pricing {
    /// Orca's `LONG_CONTEXT_THRESHOLD_TOKENS` for Codex.
    pub codex_long_context_threshold: u64,
    pub claude: Vec<ClaudePrice>,
    #[serde(default)]
    pub claude_aliases: BTreeMap<String, String>,
    pub codex: Vec<CodexPrice>,
}

/// The built-in pricing tables.
pub fn pricing() -> Pricing {
    toml_edit::de::from_str(PRICING_TOML).expect("catalog/pricing.toml parses")
}

/// Orca's `normalizeModelForPricing` for Claude, by its rules' shape:
/// aliases first, then the longest table entry the name contains after
/// `.` becomes `-` (so a point release wins over its major), and legacy
/// version-first names (`claude-3-5-sonnet-…`) turned round.
pub fn claude_model<'a>(pricing: &'a Pricing, model: &str) -> Option<&'a ClaudePrice> {
    let lower = model.trim().to_ascii_lowercase();
    let lower = lower
        .strip_prefix("anthropic/")
        .or_else(|| lower.strip_prefix("anthropic:"))
        .unwrap_or(&lower)
        .to_owned();
    if let Some(alias) = pricing.claude_aliases.get(&lower) {
        return pricing.claude.iter().find(|p| &p.model == alias);
    }
    let mut normalized = lower.replace('.', "-");
    for family in ["sonnet", "haiku", "opus"] {
        // `claude-3-5-sonnet-20241022` is `claude-sonnet-3-5`.
        for version in ["3-7", "3-5", "3"] {
            let legacy = format!("claude-{version}-{family}");
            if normalized.starts_with(&legacy) {
                normalized = format!("claude-{family}-{version}{}", &normalized[legacy.len()..]);
            }
        }
    }
    // A table entry matches where no digit follows (`opus-4-8` must not
    // claim `opus-4-80`); of several, the longest (`opus-5-5` over
    // `opus-5`) wins.
    let matches = |key: &str| {
        let bare = key.trim_start_matches("claude-");
        normalized.match_indices(bare).any(|(i, _)| {
            let after = normalized[i + bare.len()..].chars().next();
            !after.is_some_and(|c| c.is_ascii_digit())
        })
    };
    pricing
        .claude
        .iter()
        .filter(|p| matches(&p.model))
        .max_by_key(|p| p.model.len())
}

fn tiered(tokens: f64, base: f64, above: Option<f64>, threshold: Option<f64>) -> f64 {
    match (above, threshold) {
        (Some(above), Some(threshold)) => {
            tokens.min(threshold) * base + (tokens - threshold).max(0.0) * above
        }
        _ => tokens * base,
    }
}

/// Orca's `estimateCostUsd` for Claude.
pub fn claude_cost(pricing: &Pricing, turn: &Turn) -> Option<f64> {
    let p = claude_model(pricing, turn.model.as_deref()?)?;
    let write = turn.cache_write as f64;
    let write_1h = (turn.cache_write_1h as f64).clamp(0.0, write);
    let share = if write > 0.0 { write_1h / write } else { 0.0 };
    let t5 = p.threshold_tokens.map(|t| t * (1.0 - share));
    let t1 = p.threshold_tokens.map(|t| t * share);
    Some(
        (tiered(
            turn.input as f64,
            p.input,
            p.input_above,
            p.threshold_tokens,
        ) + tiered(
            turn.output as f64,
            p.output,
            p.output_above,
            p.threshold_tokens,
        ) + tiered(
            turn.cache_read as f64,
            p.cache_read,
            p.cache_read_above,
            p.threshold_tokens,
        ) + tiered(write - write_1h, p.cache_write, p.cache_write_above, t5)
            + tiered(write_1h, p.cache_write_1h, p.cache_write_1h_above, t1))
            / 1e6,
    )
}

/// Orca's `normalizeModelForPricing` for Codex: reasoning tiers stripped,
/// then an entry matched exactly or as a `-` prefix (the longest wins);
/// bare `gpt-5` only exactly (or `gpt-5-codex`), and `gpt-5.6` is Sol.
pub fn codex_model<'a>(pricing: &'a Pricing, model: &str) -> Option<&'a CodexPrice> {
    const TIERS: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "auto", "none"];
    let mut name = model.trim().to_ascii_lowercase();
    if let Some(open) = name.rfind('(') {
        if name.ends_with(')') {
            let tier = name[open + 1..name.len() - 1].trim().to_owned();
            if !(TIERS.contains(&tier.as_str()) || tier == "max" || tier == "ultra") {
                return None;
            }
            name.truncate(open);
        }
    }
    for _ in 0..4 {
        match TIERS.iter().find(|t| name.ends_with(&format!("-{t}"))) {
            Some(t) => name.truncate(name.len() - t.len() - 1),
            None => break,
        }
    }
    let wanted = match name.as_str() {
        "gpt-5" | "gpt-5-codex" => "gpt-5",
        "gpt-5.6" => "gpt-5.6-sol",
        _ => "",
    };
    if !wanted.is_empty() {
        return pricing.codex.iter().find(|p| p.model == wanted);
    }
    pricing
        .codex
        .iter()
        .filter(|p| p.model != "gpt-5")
        .filter(|p| name == p.model || name.starts_with(&format!("{}-", p.model)))
        .max_by_key(|p| p.model.len())
}

/// A Codex request's cost: long-context rates for the whole request when
/// its input is over the threshold.
pub fn codex_cost(pricing: &Pricing, event: &CodexTokens) -> Option<f64> {
    let p = codex_model(pricing, event.model.as_deref()?)?;
    let rates = match (
        &p.long_context,
        event.input > pricing.codex_long_context_threshold,
    ) {
        (Some(long), true) => long.clone(),
        _ => CodexRates {
            input: p.input,
            cached_input: p.cached_input,
            output: p.output,
        },
    };
    let cached = event.cached.min(event.input);
    Some(
        ((event.input - cached) as f64 * rates.input
            + cached as f64 * rates.cached_input
            + event.output as f64 * rates.output)
            / 1e6,
    )
}

// Reading the files.

/// Milliseconds since the epoch for an RFC 3339 time.
pub fn rfc3339_ms(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let num = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, s) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut rest = &text[19..];
    let mut ms = 0i64;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits: String = frac.chars().take_while(char::is_ascii_digit).collect();
        let mut padded: String = digits.chars().take(3).collect();
        while padded.len() < 3 {
            padded.push('0');
        }
        ms = padded.parse().ok()?;
        rest = &frac[digits.len()..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let oh: i64 = rest.get(1..3)?.parse().ok()?;
            let om: i64 = rest.get(4..6)?.parse().ok()?;
            sign * (oh * 3600 + om * 60)
        }
    };
    let days = days_from_civil(y, mo, d);
    let secs = days * 86_400 + h * 3600 + mi * 60 + s - offset;
    u64::try_from(secs * 1000 + ms).ok()
}

/// Howard Hinnant's days from the civil date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let (y, m) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `2026-10-01 14:30 UTC`.
pub fn utc(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60
    )
}

/// `2h 13m`, `4d 2h`, `12m`.
pub fn span(ms: u64) -> String {
    let minutes = ms.div_ceil(60_000);
    match minutes {
        0..=59 => format!("{minutes}m"),
        60..=1439 => format!("{}h {}m", minutes / 60, minutes % 60),
        _ => format!("{}d {}h", minutes / 1440, minutes % 1440 / 60),
    }
}

/// Every `*.jsonl` under `root` modified since `since_ms`, as Orca's
/// `walkJsonlFiles` finds them; symbolic links are not followed.
fn jsonl_files(root: &Path, since_ms: u64, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            jsonl_files(&path, since_ms, out);
        } else if kind.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(u64::MAX, |d| d.as_millis() as u64);
            if modified >= since_ms {
                out.push(path);
            }
        }
    }
}

fn lines(path: &Path) -> impl Iterator<Item = String> {
    fs::File::open(path)
        .ok()
        .map(BufReader::new)
        .into_iter()
        .flat_map(|reader| reader.lines().map_while(Result::ok))
}

/// One assistant record's usage, as Orca's `parseClaudeUsageSourceRecord`
/// reads it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Turn {
    pub at_ms: u64,
    pub model: Option<String>,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cache_write_1h: u64,
    /// Orca's dedupe key: message and request IDs, else the row's UUID.
    pub key: Option<String>,
}

impl Turn {
    /// What counts against a window here: everything but cache reads,
    /// which are billed at a tenth and would swamp the rest.
    pub fn tokens(&self) -> u64 {
        self.input + self.output + self.cache_write
    }
}

/// A "usage limit reached" message Claude Code wrote into a transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LimitHit {
    pub at_ms: u64,
    /// When it said the limit resets (`…limit reached|<epoch seconds>`).
    pub resets_at_ms: Option<u64>,
}

fn count(value: &Value) -> u64 {
    value.as_u64().unwrap_or(0)
}

/// Parse one transcript line: a turn with usage, a limit message, or
/// nothing.
pub fn claude_record(line: &str) -> (Option<Turn>, Option<LimitHit>) {
    // Orca's prefilter: only assistant records carry usage, and the others
    // can be whole files.
    if !line.contains("assistant") {
        return (None, None);
    }
    let Ok(record) = serde_json::from_str::<Value>(line) else {
        return (None, None);
    };
    if record["type"] != "assistant" {
        return (None, None);
    }
    let Some(at_ms) = record["timestamp"].as_str().and_then(rfc3339_ms) else {
        return (None, None);
    };
    let message = &record["message"];
    let hit = message["content"].as_array().and_then(|content| {
        content.iter().find_map(|part| {
            let text = part["text"].as_str()?;
            let lower = text.to_ascii_lowercase();
            if !(lower.contains("limit reached") || lower.contains("hit your limit")) {
                return None;
            }
            let resets_at_ms = text
                .split_once('|')
                .and_then(|(_, epoch)| epoch.trim().parse::<u64>().ok())
                .map(|secs| match secs > 10_000_000_000 {
                    true => secs,
                    false => secs * 1000,
                });
            Some(LimitHit {
                at_ms,
                resets_at_ms,
            })
        })
    });
    let usage = &message["usage"];
    let cache_write = count(&usage["cache_creation_input_tokens"]);
    let turn = Turn {
        at_ms,
        model: message["model"].as_str().map(str::to_owned),
        input: count(&usage["input_tokens"]),
        output: count(&usage["output_tokens"]),
        cache_read: count(&usage["cache_read_input_tokens"]),
        cache_write,
        cache_write_1h: count(&usage["cache_creation"]["ephemeral_1h_input_tokens"])
            .min(cache_write),
        key: {
            let id = message["id"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let request = record["requestId"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            match (id, request) {
                (Some(id), Some(request)) => Some(format!("{id}:{request}")),
                (Some(id), None) => Some(format!("msg:{id}")),
                _ => record["uuid"].as_str().map(|u| format!("uuid:{u}")),
            }
        },
    };
    let has_usage = turn.input + turn.output + turn.cache_read + turn.cache_write > 0;
    (has_usage.then_some(turn), hit)
}

/// Orca's `dedupeClaudeUsageTurns`: a streamed message repeats with the
/// same IDs; the later rows can carry more complete usage.
fn dedupe(turns: Vec<Turn>) -> Vec<Turn> {
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<Turn> = Vec::new();
    for turn in turns {
        if let Some(key) = &turn.key {
            if let Some(&i) = index.get(key) {
                let kept = &mut out[i];
                kept.input = kept.input.max(turn.input);
                kept.output = kept.output.max(turn.output);
                kept.cache_read = kept.cache_read.max(turn.cache_read);
                kept.cache_write = kept.cache_write.max(turn.cache_write);
                kept.cache_write_1h = kept.cache_write_1h.max(turn.cache_write_1h);
                continue;
            }
            index.insert(key.clone(), out.len());
        }
        out.push(turn);
    }
    out
}

/// One Codex `token_count` event's tokens.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CodexTokens {
    pub at_ms: u64,
    pub model: Option<String>,
    pub input: u64,
    pub cached: u64,
    pub output: u64,
    pub total: u64,
}

/// A Codex rate-limit window as a rollout reports it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CodexWindow {
    pub used_percent: f64,
    pub minutes: Option<f64>,
    pub resets_at_ms: Option<u64>,
}

/// The rate limits of one `token_count` event.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CodexLimits {
    pub at_ms: u64,
    pub primary: Option<CodexWindow>,
    pub secondary: Option<CodexWindow>,
}

fn codex_window(value: &Value, at_ms: u64) -> Option<CodexWindow> {
    let used = value
        .get("used_percent")
        .or_else(|| value.get("usedPercent"))?
        .as_f64()?;
    if !used.is_finite() {
        return None;
    }
    let minutes = value
        .get("window_minutes")
        .or_else(|| value.get("windowDurationMins"))
        .and_then(Value::as_f64);
    // Codex gives the reset in Unix seconds (Orca's mapper); older
    // rollouts gave seconds from the event instead.
    let resets_at_ms = value
        .get("resets_at")
        .or_else(|| value.get("resetsAt"))
        .and_then(Value::as_f64)
        .filter(|s| s.is_finite() && *s > 0.0)
        .map(|s| (s * 1000.0) as u64)
        .or_else(|| {
            value
                .get("resets_in_seconds")
                .and_then(Value::as_f64)
                .filter(|s| s.is_finite() && *s >= 0.0)
                .map(|s| at_ms + (s * 1000.0) as u64)
        });
    Some(CodexWindow {
        used_percent: used.clamp(0.0, 100.0),
        minutes,
        resets_at_ms,
    })
}

/// Orca's `classifyCodexRateLimitWindows`: by duration, falling back to
/// primary as the 5-hour window and secondary as the weekly one when a
/// duration is unknown.
pub fn classify(
    primary: Option<&CodexWindow>,
    secondary: Option<&CodexWindow>,
) -> (Option<CodexWindow>, Option<CodexWindow>) {
    let kind = |w: &CodexWindow| -> Option<bool> {
        let minutes = w.minutes.filter(|m| m.is_finite())?;
        if (minutes - f64::from(FIVE_HOUR_MINUTES)).abs() <= WINDOW_TOLERANCE_MINUTES {
            Some(true)
        } else if (minutes - f64::from(WEEKLY_MINUTES)).abs() <= WINDOW_TOLERANCE_MINUTES {
            Some(false)
        } else {
            None
        }
    };
    let (mut session, mut weekly) = (None, None);
    for window in [primary, secondary].into_iter().flatten() {
        match kind(window) {
            Some(true) if session.is_none() => session = Some(window.clone()),
            Some(false) if weekly.is_none() => weekly = Some(window.clone()),
            _ => {}
        }
    }
    if session.is_none() {
        session = primary.filter(|w| kind(w).is_none()).cloned();
    }
    if weekly.is_none() {
        weekly = secondary.filter(|w| kind(w).is_none()).cloned();
    }
    (session, weekly)
}

fn usage_of(value: &Value) -> Option<(u64, u64, u64, u64)> {
    value.is_object().then(|| {
        (
            count(&value["input_tokens"]),
            count(&value["cached_input_tokens"]),
            count(&value["output_tokens"]),
            count(&value["total_tokens"]),
        )
    })
}

/// One rollout file: its token events and its latest rate limits. A
/// request's tokens are `last_token_usage`, else the change in
/// `total_token_usage` (Orca's delta rule).
pub fn codex_file(path: &Path) -> (Vec<CodexTokens>, Option<CodexLimits>) {
    let mut events = Vec::new();
    let mut limits: Option<CodexLimits> = None;
    let mut model: Option<String> = None;
    let mut previous: Option<(u64, u64, u64, u64)> = None;
    for line in lines(path) {
        if !(line.contains("token_count") || line.contains("turn_context")) {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let payload = &record["payload"];
        if record["type"] == "turn_context" {
            if let Some(m) = payload["model"].as_str() {
                model = Some(m.to_owned());
            }
            continue;
        }
        if record["type"] != "event_msg" || payload["type"] != "token_count" {
            continue;
        }
        let Some(at_ms) = record["timestamp"].as_str().and_then(rfc3339_ms) else {
            continue;
        };
        let rate_limits = &payload["rate_limits"];
        if rate_limits.is_object() {
            let snapshot = CodexLimits {
                at_ms,
                primary: codex_window(&rate_limits["primary"], at_ms),
                secondary: codex_window(&rate_limits["secondary"], at_ms),
            };
            if snapshot.primary.is_some() || snapshot.secondary.is_some() {
                limits = Some(snapshot);
            }
        }
        // A token_count with no info is a rate-limit update only.
        let info = &payload["info"];
        let total = usage_of(&info["total_token_usage"]);
        let last = usage_of(&info["last_token_usage"]);
        let delta = match (last, total, previous) {
            (Some(last), _, _) => Some(last),
            (None, Some(t), Some(p)) => Some((
                t.0.saturating_sub(p.0),
                t.1.saturating_sub(p.1),
                t.2.saturating_sub(p.2),
                t.3.saturating_sub(p.3),
            )),
            // The first total is a baseline.
            _ => None,
        };
        if total.is_some() {
            previous = total;
        }
        if let Some((input, cached, output, total)) = delta {
            if input + output + total > 0 {
                events.push(CodexTokens {
                    at_ms,
                    model: info["model"]
                        .as_str()
                        .map(str::to_owned)
                        .or_else(|| model.clone()),
                    input,
                    cached: cached.min(input),
                    output,
                    total: total.max(input + output),
                });
            }
        }
    }
    (events, limits)
}

// Meters.

/// One window of one login.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Window {
    /// `five_hour` or `weekly`.
    pub window: &'static str,
    pub minutes: u32,
    /// How much is used, when known: reported (Codex), configured against
    /// a budget (Claude), or 100 after a limit message.
    pub used_percent: Option<f64>,
    /// When it resets, when known.
    pub resets_at_ms: Option<u64>,
    /// Tokens in the window, from the session files (cache reads left out).
    pub tokens: u64,
    pub cost_usd: Option<f64>,
    /// The harness said its limit was reached in this window.
    pub limit_reached: bool,
    /// Where `used_percent` came from: `rate_limits` (the harness's own
    /// report), `budget` (`[usage]`), `limit_message`, or `none`.
    pub source: &'static str,
}

impl Window {
    fn new(window: &'static str, minutes: u32) -> Window {
        Window {
            window,
            minutes,
            used_percent: None,
            resets_at_ms: None,
            tokens: 0,
            cost_usd: None,
            limit_reached: false,
            source: "none",
        }
    }

    fn add_cost(&mut self, cost: Option<f64>) {
        if let Some(cost) = cost {
            self.cost_usd = Some(self.cost_usd.unwrap_or(0.0) + cost);
        }
    }

    fn label(&self) -> &'static str {
        match self.window {
            "five_hour" => "5-hour",
            _ => "weekly",
        }
    }
}

/// One login's meters.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Login {
    /// `claude-code` or `codex`.
    pub harness: &'static str,
    /// `default`, or the name in `[usage.accounts]`.
    pub account: String,
    /// The configuration directory read.
    pub dir: String,
    /// Whether it has session files at all.
    pub found: bool,
    /// The newest record read.
    pub last_seen_ms: Option<u64>,
    pub five_hour: Window,
    pub weekly: Window,
    pub notes: Vec<String>,
}

impl Login {
    /// `claude-code` or `claude-code (work)`.
    pub fn label(&self) -> String {
        match self.account.as_str() {
            "default" => self.harness.to_owned(),
            name => format!("{} ({name})", self.harness),
        }
    }

    /// The fuller of its windows over `percent`, as a sentence; `None`
    /// when neither is.
    pub fn over(&self, percent: f64, now_ms: u64) -> Option<String> {
        [&self.five_hour, &self.weekly]
            .into_iter()
            .filter(|w| w.limit_reached || w.used_percent.is_some_and(|u| u >= percent))
            .max_by(|a, b| {
                a.used_percent
                    .unwrap_or(100.0)
                    .total_cmp(&b.used_percent.unwrap_or(100.0))
            })
            .map(|w| {
                let used = match (w.limit_reached, w.used_percent) {
                    (true, _) => "reached".to_owned(),
                    (false, Some(u)) => format!("used {u:.0}% of"),
                    (false, None) => "used".to_owned(),
                };
                let resets = match w.resets_at_ms {
                    Some(at) if at > now_ms => format!("; it resets in {}", span(at - now_ms)),
                    _ => String::new(),
                };
                format!(
                    "the {} login has {used} its {} window{resets}",
                    self.label(),
                    w.label()
                )
            })
    }
}

/// The default configuration directory of a harness's login.
pub fn default_dir(harness: &str, env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let (var, name) = match harness {
        "claude-code" => ("CLAUDE_CONFIG_DIR", ".claude"),
        "codex" => ("CODEX_HOME", ".codex"),
        _ => return None,
    };
    match env(var).filter(|v| !v.trim().is_empty()) {
        Some(dir) => Some(PathBuf::from(dir)),
        None => env("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .or_else(dirs::home_dir)
            .map(|home| home.join(name)),
    }
}

fn expand(dir: &str, env: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    match dir.strip_prefix("~/") {
        Some(rest) => env("HOME")
            .map(PathBuf::from)
            .or_else(dirs::home_dir)
            .unwrap_or_default()
            .join(rest),
        None => PathBuf::from(dir),
    }
}

/// Claude Code's meters from the transcripts under `dir` (Orca's roots:
/// `projects/` and `transcripts/`). The 5-hour window starts at the hour
/// of the first message after the last one ended and lasts five hours; the
/// weekly one is the last seven days (Claude's own weekly reset is not on
/// disk).
pub fn claude(
    account: &str,
    dir: &Path,
    config: &UsageConfig,
    pricing: &Pricing,
    now_ms: u64,
) -> Login {
    let since = now_ms.saturating_sub(SCAN_HORIZON_MS);
    let mut files = Vec::new();
    for root in ["projects", "transcripts"] {
        jsonl_files(&dir.join(root), since, &mut files);
    }
    files.sort();
    let mut turns = Vec::new();
    let mut hit: Option<LimitHit> = None;
    for file in &files {
        let mut in_file = Vec::new();
        for line in lines(file) {
            let (turn, limit) = claude_record(&line);
            in_file.extend(turn);
            if let Some(limit) = limit {
                if hit.is_none_or(|h| limit.at_ms > h.at_ms) {
                    hit = Some(limit);
                }
            }
        }
        turns.extend(dedupe(in_file));
    }
    // Forks copy history across files: one message counts once.
    let mut turns = dedupe(turns);
    turns.retain(|t| t.at_ms + WEEK_MS + FIVE_HOURS_MS > now_ms && t.at_ms <= now_ms);
    turns.sort_by_key(|t| t.at_ms);
    let mut five = Window::new("five_hour", FIVE_HOUR_MINUTES);
    let mut weekly = Window::new("weekly", WEEKLY_MINUTES);
    let mut block: Option<(u64, u64)> = None;
    let mut block_turns: Vec<&Turn> = Vec::new();
    for turn in &turns {
        if block.is_none_or(|(_, end)| turn.at_ms >= end) {
            let start = turn.at_ms - turn.at_ms % HOUR_MS;
            block = Some((start, start + FIVE_HOURS_MS));
            block_turns.clear();
        }
        block_turns.push(turn);
        if turn.at_ms + WEEK_MS > now_ms {
            weekly.tokens += turn.tokens();
            weekly.add_cost(claude_cost(pricing, turn));
        }
    }
    let mut notes = Vec::new();
    if let Some((_, end)) = block.filter(|(_, end)| *end > now_ms) {
        five.resets_at_ms = Some(end);
        for turn in &block_turns {
            five.tokens += turn.tokens();
            five.add_cost(claude_cost(pricing, turn));
        }
    }
    for (window, budget) in [
        (&mut five, config.claude_five_hour_tokens),
        (&mut weekly, config.claude_weekly_tokens),
    ] {
        if let Some(budget) = budget.filter(|b| *b > 0) {
            window.used_percent = Some((window.tokens as f64 * 100.0 / budget as f64).min(100.0));
            window.source = "budget";
        }
    }
    if let Some(hit) = hit {
        let open = match hit.resets_at_ms {
            Some(reset) => reset > now_ms,
            None => five
                .resets_at_ms
                .is_some_and(|end| hit.at_ms + FIVE_HOURS_MS > now_ms && end > hit.at_ms),
        };
        if open {
            five.limit_reached = true;
            five.used_percent = Some(100.0);
            five.source = "limit_message";
            if let Some(reset) = hit.resets_at_ms {
                five.resets_at_ms = Some(reset);
            }
        }
    }
    if config.claude_five_hour_tokens.is_none() && !five.limit_reached {
        notes.push(
            "Claude Code records no limits on disk: the percent needs [usage] \
             claude_five_hour_tokens (and claude_weekly_tokens)"
                .into(),
        );
    }
    Login {
        harness: "claude-code",
        account: account.to_owned(),
        dir: dir.display().to_string(),
        found: dir.join("projects").is_dir() || dir.join("transcripts").is_dir(),
        last_seen_ms: turns.last().map(|t| t.at_ms).max(hit.map(|h| h.at_ms)),
        five_hour: five,
        weekly,
        notes,
    }
}

/// Codex's meters from the rollouts under `dir` (`sessions/` and
/// `archived_sessions/`): the latest rate limits any of them reported,
/// mapped as Orca maps them, with tokens and cost summed from the same
/// files.
pub fn codex(account: &str, dir: &Path, pricing: &Pricing, now_ms: u64) -> Login {
    let since = now_ms.saturating_sub(SCAN_HORIZON_MS);
    let mut files = Vec::new();
    for root in ["sessions", "archived_sessions"] {
        jsonl_files(&dir.join(root), since, &mut files);
    }
    files.sort();
    let mut events = Vec::new();
    let mut latest: Option<CodexLimits> = None;
    for file in &files {
        let (more, limits) = codex_file(file);
        events.extend(more);
        if let Some(limits) = limits {
            if latest.as_ref().is_none_or(|l| limits.at_ms > l.at_ms) {
                latest = Some(limits);
            }
        }
    }
    let mut five = Window::new("five_hour", FIVE_HOUR_MINUTES);
    let mut weekly = Window::new("weekly", WEEKLY_MINUTES);
    let mut notes = Vec::new();
    if let Some(limits) = &latest {
        let (session, week) = classify(limits.primary.as_ref(), limits.secondary.as_ref());
        for (window, reported) in [(&mut five, session), (&mut weekly, week)] {
            let Some(reported) = reported else { continue };
            window.source = "rate_limits";
            match reported.resets_at_ms {
                // The window reset since Codex last said: nothing of it is
                // used that this machine knows of.
                Some(reset) if reset <= now_ms => {
                    window.used_percent = Some(0.0);
                    notes.push(format!(
                        "its {} window reset at {} after Codex last reported it",
                        window.label(),
                        utc(reset)
                    ));
                }
                reset => {
                    window.used_percent = Some(reported.used_percent);
                    window.resets_at_ms = reset;
                    window.limit_reached = reported.used_percent >= 100.0;
                }
            }
        }
    } else if !files.is_empty() {
        notes.push("no rollout in the last week reported rate limits".into());
    }
    for event in events.iter().filter(|e| e.at_ms <= now_ms) {
        let starts = |w: &Window, length: u64| match w.resets_at_ms {
            Some(reset) => reset.saturating_sub(length),
            None => now_ms.saturating_sub(length),
        };
        let cost = codex_cost(pricing, event);
        if event.at_ms >= starts(&five, FIVE_HOURS_MS) {
            five.tokens += event.total;
            five.add_cost(cost);
        }
        if event.at_ms >= starts(&weekly, WEEK_MS) {
            weekly.tokens += event.total;
            weekly.add_cost(cost);
        }
    }
    Login {
        harness: "codex",
        account: account.to_owned(),
        dir: dir.display().to_string(),
        found: dir.join("sessions").is_dir() || dir.join("archived_sessions").is_dir(),
        last_seen_ms: events
            .iter()
            .map(|e| e.at_ms)
            .max()
            .max(latest.map(|l| l.at_ms)),
        five_hour: five,
        weekly,
        notes,
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Every login to meter: the default Claude Code and Codex logins, then
/// `[usage.accounts]`.
pub fn meter(
    config: &UsageConfig,
    env: &dyn Fn(&str) -> Option<String>,
    now_ms: u64,
) -> Vec<Login> {
    let pricing = pricing();
    let mut out = Vec::new();
    for harness in ["claude-code", "codex"] {
        if let Some(dir) = default_dir(harness, env) {
            out.push(read(harness, "default", &dir, config, &pricing, now_ms));
        }
    }
    for (name, account) in &config.accounts {
        let dir = expand(&account.dir, env);
        out.push(read(&account.harness, name, &dir, config, &pricing, now_ms));
    }
    out
}

fn read(
    harness: &str,
    account: &str,
    dir: &Path,
    config: &UsageConfig,
    pricing: &Pricing,
    now_ms: u64,
) -> Login {
    match harness {
        "codex" => codex(account, dir, pricing, now_ms),
        _ => claude(account, dir, config, pricing, now_ms),
    }
}

/// The login a harness or profile ID uses: `claude-code` or `codex`.
pub fn login_of(harness: &str) -> Option<&'static str> {
    match harness {
        h if h.starts_with("claude-code") || h == "claude" => Some("claude-code"),
        h if h.starts_with("codex") => Some("codex"),
        _ => None,
    }
}

// Commands.

/// `by usage [--json]`.
pub fn show(env: &crate::commands::Env, as_json: bool) -> crate::commands::Outcome {
    let cwd = std::env::current_dir()?;
    let vars = |name: &str| std::env::var(name).ok();
    let config = crate::defaults::config_at(&cwd, &vars)
        .map_err(crate::commands::Failure::Message)?
        .map(|c| c.usage)
        .unwrap_or_default();
    let now = now_ms();
    let logins = meter(&config, &vars, now);
    if as_json {
        let value = serde_json::json!({
            "now_ms": now,
            "near_percent": config.near_percent.unwrap_or(DEFAULT_NEAR_PERCENT),
            "logins": logins,
        });
        return crate::commands::print(&crate::json::text(&value));
    }
    crate::commands::print(&table(&logins, &config, now, env.style()))
}

fn percent_text(w: &Window) -> String {
    match (w.limit_reached, w.used_percent) {
        (true, _) => "full".into(),
        (false, Some(p)) => format!("{p:.0}%"),
        (false, None) => "-".into(),
    }
}

fn reset_text(w: &Window, now: u64) -> String {
    match w.resets_at_ms {
        Some(at) if at > now => format!("in {} ({})", span(at - now), utc(at)),
        _ if w.window == "five_hour" && w.source != "rate_limits" => "no window open".into(),
        _ if w.source == "rate_limits" => "-".into(),
        _ => "rolling 7 days".into(),
    }
}

/// `by usage`'s table.
pub fn table(logins: &[Login], config: &UsageConfig, now: u64, style: Style) -> String {
    let near = config.near_percent.unwrap_or(DEFAULT_NEAR_PERCENT);
    let mut rows: Vec<[String; 6]> = vec![[
        "LOGIN".into(),
        "WINDOW".into(),
        "USED".into(),
        "RESETS".into(),
        "TOKENS".into(),
        "COST".into(),
    ]];
    let mut notes = Vec::new();
    for login in logins {
        if !login.found {
            notes.push(format!(
                "{}: no session files under {}",
                login.label(),
                login.dir
            ));
            continue;
        }
        for (i, w) in [&login.five_hour, &login.weekly].into_iter().enumerate() {
            rows.push([
                if i == 0 { login.label() } else { String::new() },
                w.label().into(),
                percent_text(w),
                reset_text(w, now),
                render::tokens(w.tokens),
                render::cost_text(w.cost_usd),
            ]);
        }
        for note in &login.notes {
            notes.push(format!("{}: {note}", login.label()));
        }
    }
    let widths: Vec<usize> = (0..6)
        .map(|c| rows.iter().map(|r| r[c].chars().count()).max().unwrap_or(0))
        .collect();
    let mut out = String::new();
    for (n, row) in rows.iter().enumerate() {
        let mut line = String::new();
        for (c, cell) in row.iter().enumerate() {
            let pad = widths[c] - cell.chars().count();
            let cell = match (n, c) {
                (0, _) => style.paint(Tone::Dim, cell),
                (_, 2)
                    if cell == "full"
                        || cell
                            .trim_end_matches('%')
                            .parse::<f64>()
                            .is_ok_and(|p| p >= near) =>
                {
                    style.paint(Tone::Red, cell)
                }
                _ => cell.clone(),
            };
            match c {
                4 | 5 => line.push_str(&format!("{}{cell}  ", " ".repeat(pad))),
                _ => line.push_str(&format!("{cell}{}  ", " ".repeat(pad))),
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    if rows.len() == 1 {
        out.push_str("no Claude Code or Codex session files found\n");
    }
    for note in notes {
        out.push_str(&style.paint(Tone::Dim, &format!("{note}\n")));
    }
    out
}

/// One line for `by watch`'s header: each found login's fuller window.
pub fn header(logins: &[Login]) -> Option<String> {
    let parts: Vec<String> = logins
        .iter()
        .filter(|l| l.found)
        .map(|l| {
            let window = |w: &Window, short: &str| match (w.limit_reached, w.used_percent) {
                (true, _) => Some(format!("{short} full")),
                (false, Some(p)) => Some(format!("{short} {p:.0}%")),
                (false, None) if w.tokens > 0 => {
                    Some(format!("{short} {}", render::tokens(w.tokens)))
                }
                _ => None,
            };
            let windows: Vec<String> = [window(&l.five_hour, "5h"), window(&l.weekly, "wk")]
                .into_iter()
                .flatten()
                .collect();
            match windows.is_empty() {
                true => format!("{} idle", l.label()),
                false => format!("{} {}", l.label(), windows.join(" ")),
            }
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// What `by run` and `by fan` do about `harnesses` near a limit: say so
/// (`warn`), refuse (`refuse`), or nothing (`off`). Only each harness's
/// default login is checked (the one it runs with unless its
/// configuration directory is changed).
pub fn guard(harnesses: &[String]) -> Result<(), crate::commands::Failure> {
    let cwd = std::env::current_dir()?;
    let vars = |name: &str| std::env::var(name).ok();
    let Some(config) = crate::defaults::config_at(&cwd, &vars)
        .map_err(crate::commands::Failure::Message)?
        .map(|c| c.usage)
    else {
        return check(
            &UsageConfig::default(),
            harnesses,
            &vars,
            now_ms(),
            &mut |w| eprintln!("by: warning: {w}"),
        );
    };
    check(&config, harnesses, &vars, now_ms(), &mut |w| {
        eprintln!("by: warning: {w}")
    })
}

/// [`guard`]'s decision, with what to warn about passed to `warn`.
pub fn check(
    config: &UsageConfig,
    harnesses: &[String],
    env: &dyn Fn(&str) -> Option<String>,
    now_ms: u64,
    warn: &mut dyn FnMut(&str),
) -> Result<(), crate::commands::Failure> {
    let mode = config.guard.unwrap_or(UsageGuard::Warn);
    if mode == UsageGuard::Off {
        return Ok(());
    }
    let near = config.near_percent.unwrap_or(DEFAULT_NEAR_PERCENT);
    let mut logins: Vec<&'static str> = harnesses.iter().filter_map(|h| login_of(h)).collect();
    logins.sort();
    logins.dedup();
    if logins.is_empty() {
        return Ok(());
    }
    let pricing = pricing();
    for harness in logins {
        let Some(dir) = default_dir(harness, env) else {
            continue;
        };
        let login = read(harness, "default", &dir, config, &pricing, now_ms);
        if let Some(why) = login.over(near, now_ms) {
            match mode {
                UsageGuard::Refuse => {
                    return Err(crate::commands::Failure::Message(format!(
                        "{why}; [usage] guard = \"refuse\" stops new branches on it (by usage \
                         shows every window)"
                    )))
                }
                _ => warn(&format!("{why} (by usage)")),
            }
        }
    }
    Ok(())
}

/// For the router: each fleet candidate's harness whose default login has
/// used more than `[usage] skip_over` of a window, with why.
pub fn exclusions(
    config: &UsageConfig,
    candidates: &[String],
    env: &dyn Fn(&str) -> Option<String>,
    now_ms: u64,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Some(over) = config.skip_over else {
        return out;
    };
    let pricing = pricing();
    let mut seen: HashMap<&'static str, Option<String>> = HashMap::new();
    for harness in candidates {
        let Some(login) = login_of(harness) else {
            continue;
        };
        let why = seen
            .entry(login)
            .or_insert_with(|| {
                let dir = default_dir(login, env)?;
                read(login, "default", &dir, config, &pricing, now_ms).over(over, now_ms)
            })
            .clone();
        if let Some(why) = why {
            out.insert(
                harness.clone(),
                format!("{why}, over [usage] skip_over = {over}"),
            );
        }
    }
    out
}

/// For a routed `by run` or `by fan`: the candidates the router must not
/// pick, with why. `skip_over` excludes a candidate whose login is over it;
/// with `guard = "refuse"` so does `near_percent`; with `guard = "warn"`
/// (the default) a candidate near its limit is only warned about.
pub fn route_exclusions(fleet: &branchyard::Fleet) -> BTreeMap<String, String> {
    let Ok(cwd) = std::env::current_dir() else {
        return BTreeMap::new();
    };
    let vars = |name: &str| std::env::var(name).ok();
    let config = crate::defaults::config_at(&cwd, &vars)
        .ok()
        .flatten()
        .map(|c| c.usage)
        .unwrap_or_default();
    let mut candidates: Vec<String> = fleet
        .entries
        .values()
        .flat_map(|e| e.candidates.iter().map(|c| c.harness.clone()))
        .collect();
    candidates.sort();
    candidates.dedup();
    let now = now_ms();
    let mut out = exclusions(&config, &candidates, &vars, now);
    let near = config.near_percent.unwrap_or(DEFAULT_NEAR_PERCENT);
    match config.guard.unwrap_or(UsageGuard::Warn) {
        UsageGuard::Off => {}
        UsageGuard::Refuse => {
            let refused = exclusions(
                &UsageConfig {
                    skip_over: Some(near),
                    ..config.clone()
                },
                &candidates,
                &vars,
                now,
            );
            for (harness, why) in refused {
                out.entry(harness).or_insert_with(|| {
                    why.replace("skip_over", "guard = \"refuse\" at near_percent")
                });
            }
        }
        UsageGuard::Warn => {
            let near_ones = exclusions(
                &UsageConfig {
                    skip_over: Some(near),
                    ..config.clone()
                },
                &candidates,
                &vars,
                now,
            );
            for (harness, why) in near_ones {
                if !out.contains_key(&harness) {
                    let why = why
                        .split(", over [usage]")
                        .next()
                        .unwrap_or(&why)
                        .to_owned();
                    eprintln!("by: warning: candidate {harness}: {why} (by usage)");
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_790_000_000_000; // 2026-09-21 14:13:20 UTC

    #[test]
    fn times_parse_and_print() {
        assert_eq!(rfc3339_ms("1970-01-01T00:00:01.5Z"), Some(1500));
        assert_eq!(rfc3339_ms("2026-09-21T16:13:20+02:00"), Some(NOW));
        assert_eq!(utc(NOW), "2026-09-21 14:13 UTC");
        assert_eq!(span(59_000), "1m");
        assert_eq!(span(2 * HOUR_MS + 13 * 60_000), "2h 13m");
        assert_eq!(span(4 * 86_400_000 + 2 * HOUR_MS), "4d 2h");
    }

    #[test]
    fn models_find_their_prices() {
        let p = pricing();
        let claude = |m: &str| claude_model(&p, m).map(|p| p.model.as_str());
        assert_eq!(claude("claude-opus-4-8-20260101"), Some("claude-opus-4-8"));
        assert_eq!(claude("claude-opus-5-5"), Some("claude-opus-5-5"));
        assert_eq!(claude("claude-opus-5"), Some("claude-opus-5"));
        assert_eq!(
            claude("anthropic/claude-sonnet-4.6"),
            Some("claude-sonnet-4-6")
        );
        assert_eq!(
            claude("claude-3-5-sonnet-20241022"),
            Some("claude-sonnet-3-5")
        );
        assert_eq!(claude("claude-haiku-4-5"), Some("claude-haiku-4-5"));
        assert_eq!(claude("<synthetic>"), None);
        let codex = |m: &str| codex_model(&p, m).map(|p| p.model.as_str());
        assert_eq!(codex("gpt-5-codex"), Some("gpt-5"));
        assert_eq!(codex("gpt-5.1-codex-max-high"), Some("gpt-5.1-codex-max"));
        assert_eq!(codex("gpt-5.5(high)"), Some("gpt-5.5"));
        assert_eq!(codex("gpt-5.6"), Some("gpt-5.6-sol"));
        assert_eq!(codex("gpt-5-mini"), None);
        assert_eq!(codex("gpt-5.5(turbo)"), None);
        // Opus 4.8: $5 in, $25 out, $0.50 cache read, $6.25 write.
        let turn = Turn {
            model: Some("claude-opus-4-8".into()),
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 1_000_000,
            cache_write: 1_000_000,
            ..Turn::default()
        };
        assert_eq!(claude_cost(&p, &turn), Some(36.75));
        let long = CodexTokens {
            model: Some("gpt-5.5".into()),
            input: 300_000,
            cached: 100_000,
            output: 10_000,
            ..CodexTokens::default()
        };
        // Long context: $10 in, $1 cached, $45 out.
        let cost = codex_cost(&p, &long).unwrap();
        assert!((cost - (200_000.0 * 10.0 + 100_000.0 + 450_000.0) / 1e6).abs() < 1e-9);
    }

    /// The vendored sources still have the shape this port follows.
    #[test]
    fn orcas_sources_are_the_ones_ported() {
        let windows = include_str!(
            "../../../vendor/orca/src/main/rate-limits/codex-rate-limit-window-classification.ts"
        );
        assert!(windows.contains("export const CODEX_SESSION_WINDOW_MINUTES = 300"));
        assert!(windows.contains("export const CODEX_WEEKLY_WINDOW_MINUTES = 10080"));
        assert!(windows.contains("const CODEX_WINDOW_DURATION_TOLERANCE_MINUTES = 1"));
        let mapper = include_str!(
            "../../../vendor/orca/src/main/rate-limits/codex-rate-limit-window-mapper.ts"
        );
        assert!(mapper.contains("new Date(raw.resetsAt * 1000)"));
        let parser =
            include_str!("../../../vendor/orca/src/main/claude-usage/transcript-record-parser.ts");
        assert!(parser.contains("return `${messageId}:${requestId}`"));
        assert!(parser.contains("return line.includes('assistant')"));
    }

    #[test]
    fn codex_windows_classify_as_orca_does() {
        let w = |used: f64, minutes: Option<f64>| CodexWindow {
            used_percent: used,
            minutes,
            resets_at_ms: None,
        };
        // By duration, whichever slot it came in.
        let (s, wk) = classify(Some(&w(10.0, Some(10_080.0))), Some(&w(20.0, Some(299.0))));
        assert_eq!(
            (s.unwrap().used_percent, wk.unwrap().used_percent),
            (20.0, 10.0)
        );
        // Unknown durations keep primary as session, secondary as weekly.
        let (s, wk) = classify(Some(&w(1.0, None)), Some(&w(2.0, Some(42.0))));
        assert_eq!(
            (s.unwrap().used_percent, wk.unwrap().used_percent),
            (1.0, 2.0)
        );
        // A known duration is never reused as the other window.
        let (s, wk) = classify(Some(&w(1.0, Some(300.0))), None);
        assert_eq!((s.is_some(), wk.is_none()), (true, true));
    }

    #[test]
    fn claude_records_dedupe_and_limits_are_seen() {
        let line = |id: &str, out: u64| {
            format!(
                r#"{{"type":"assistant","timestamp":"2026-09-21T13:00:00Z","requestId":"r","message":{{"id":"{id}","model":"claude-opus-4-8","usage":{{"input_tokens":5,"output_tokens":{out}}}}}}}"#
            )
        };
        let turns: Vec<Turn> = [line("m1", 1), line("m1", 9), line("m2", 3)]
            .iter()
            .filter_map(|l| claude_record(l).0)
            .collect();
        let turns = dedupe(turns);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].output, 9);
        let (turn, hit) = claude_record(
            r#"{"type":"assistant","timestamp":"2026-09-21T13:00:00Z","isApiErrorMessage":true,"message":{"model":"<synthetic>","content":[{"type":"text","text":"Claude AI usage limit reached|1790003600"}],"usage":{"input_tokens":0,"output_tokens":0}}}"#,
        );
        assert_eq!(turn, None);
        assert_eq!(hit.unwrap().resets_at_ms, Some(1_790_003_600_000));
        assert_eq!(
            claude_record(r#"{"type":"user","message":"assistant"}"#),
            (None, None)
        );
    }

    #[test]
    fn logins_map_from_harnesses() {
        assert_eq!(login_of("claude-code"), Some("claude-code"));
        assert_eq!(login_of("claude-code-stream-json"), Some("claude-code"));
        assert_eq!(login_of("codex-app-server"), Some("codex"));
        assert_eq!(login_of("gemini-cli"), None);
    }
}
