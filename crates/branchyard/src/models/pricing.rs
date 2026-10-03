// Derived from stablyai/orca at revision
// 280733273545f0b3eeedc1be54b14d406239030e:
// src/main/claude-usage/claude-model-pricing.ts and
// src/main/codex-usage/codex-model-pricing.ts.
// Copyright (c) 2026 Lovecast Inc. Licensed under the MIT License; the
// license text, which must accompany substantial portions of this code, is
// in vendor/orca/LICENSE.
// Modified for Branchyard: translated from TypeScript to Rust as in
// crates/branchyard-cli/src/usage.rs, over a metered call's tokens (cache
// reads and writes counted apart) instead of a transcript's; the tables are
// data in catalog/pricing.toml.

//! What a model's tokens cost, from `catalog/pricing.toml`: the gateway's
//! exact cost for a call it metered.
//!
//! The tables and the model-name rules are the ones `by usage` estimates
//! with (`crates/branchyard-cli/src/usage.rs`, after Orca's): Claude's
//! names by the longest table entry they contain, with Claude's
//! long-context tier above its threshold; OpenAI's by an exact or `-`
//! prefixed entry, with long-context rates for a request whose input is
//! over the threshold.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::Deserialize;

use super::{Api, Tokens};

const PRICING_TOML: &str = include_str!("../../../../catalog/pricing.toml");

#[derive(Clone, Debug, Deserialize)]
pub struct ClaudePrice {
    pub model: String,
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub cache_write_1h: f64,
    #[serde(default)]
    pub threshold_tokens: Option<f64>,
    #[serde(default)]
    pub input_above: Option<f64>,
    #[serde(default)]
    pub output_above: Option<f64>,
    #[serde(default)]
    pub cache_read_above: Option<f64>,
    #[serde(default)]
    pub cache_write_above: Option<f64>,
    #[serde(default)]
    pub cache_write_1h_above: Option<f64>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct OpenaiRates {
    pub input: f64,
    pub cached_input: f64,
    pub output: f64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct OpenaiPrice {
    pub model: String,
    pub input: f64,
    pub cached_input: f64,
    pub output: f64,
    #[serde(default)]
    pub long_context: Option<OpenaiRates>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Pricing {
    pub codex_long_context_threshold: u64,
    pub claude: Vec<ClaudePrice>,
    #[serde(default)]
    pub claude_aliases: BTreeMap<String, String>,
    #[serde(rename = "codex")]
    pub openai: Vec<OpenaiPrice>,
}

/// The built-in tables, parsed once.
pub fn pricing() -> &'static Pricing {
    static PRICING: OnceLock<Pricing> = OnceLock::new();
    PRICING
        .get_or_init(|| toml_edit::de::from_str(PRICING_TOML).expect("catalog/pricing.toml parses"))
}

/// The Claude entry for `model`: an alias, else the longest entry the
/// name contains (dots read as dashes, legacy version-first names turned
/// round) where no digit follows it.
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
        for version in ["3-7", "3-5", "3"] {
            let legacy = format!("claude-{version}-{family}");
            if normalized.starts_with(&legacy) {
                normalized = format!("claude-{family}-{version}{}", &normalized[legacy.len()..]);
            }
        }
    }
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

/// The OpenAI entry for `model`: reasoning tiers stripped, then an entry
/// matched exactly or as a `-` prefix (the longest wins); bare `gpt-5`
/// only exactly (or `gpt-5-codex`), and `gpt-5.6` is Sol.
pub fn openai_model<'a>(pricing: &'a Pricing, model: &str) -> Option<&'a OpenaiPrice> {
    const TIERS: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "auto", "none"];
    let mut name = model.trim().to_ascii_lowercase();
    if let Some(open) = name.rfind('(') {
        if name.ends_with(')') {
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
        return pricing.openai.iter().find(|p| p.model == wanted);
    }
    pricing
        .openai
        .iter()
        .filter(|p| p.model != "gpt-5")
        .filter(|p| name == p.model || name.starts_with(&format!("{}-", p.model)))
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

/// What `tokens` of `model` cost on `api`, by the catalog; `None` when it
/// does not price the model. A generic backend's model is looked up as
/// Claude's, then OpenAI's.
pub fn cost(api: Api, model: &str, tokens: &Tokens) -> Option<f64> {
    let pricing = pricing();
    match api {
        Api::Anthropic => claude_cost(pricing, model, tokens),
        Api::Openai => openai_cost(pricing, model, tokens),
        Api::Generic => {
            claude_cost(pricing, model, tokens).or_else(|| openai_cost(pricing, model, tokens))
        }
    }
}

fn claude_cost(pricing: &Pricing, model: &str, t: &Tokens) -> Option<f64> {
    let p = claude_model(pricing, model)?;
    let write = (t.cache_write + t.cache_write_1h) as f64;
    let write_1h = t.cache_write_1h as f64;
    let share = if write > 0.0 { write_1h / write } else { 0.0 };
    let t5 = p.threshold_tokens.map(|x| x * (1.0 - share));
    let t1 = p.threshold_tokens.map(|x| x * share);
    Some(
        (tiered(t.input as f64, p.input, p.input_above, p.threshold_tokens)
            + tiered(
                t.output as f64,
                p.output,
                p.output_above,
                p.threshold_tokens,
            )
            + tiered(
                t.cache_read as f64,
                p.cache_read,
                p.cache_read_above,
                p.threshold_tokens,
            )
            + tiered(write - write_1h, p.cache_write, p.cache_write_above, t5)
            + tiered(write_1h, p.cache_write_1h, p.cache_write_1h_above, t1))
            / 1e6,
    )
}

fn openai_cost(pricing: &Pricing, model: &str, t: &Tokens) -> Option<f64> {
    let p = openai_model(pricing, model)?;
    let input = t.input + t.cache_read;
    let rates = match (
        &p.long_context,
        input > pricing.codex_long_context_threshold,
    ) {
        (Some(long), true) => long.clone(),
        _ => OpenaiRates {
            input: p.input,
            cached_input: p.cached_input,
            output: p.output,
        },
    };
    Some(
        (t.input as f64 * rates.input
            + t.cache_read as f64 * rates.cached_input
            + t.output as f64 * rates.output)
            / 1e6,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalog_prices_both_families_and_says_when_it_cannot() {
        let tokens = Tokens {
            input: 1_000_000,
            output: 100_000,
            cache_read: 1_000_000,
            cache_write: 1_000_000,
            cache_write_1h: 0,
        };
        // Sonnet 4.6: $3 in, $15 out, $0.30 read, $3.75 write per million.
        let cost = cost(Api::Anthropic, "claude-sonnet-4-6", &tokens).unwrap();
        assert!((cost - (3.0 + 1.5 + 0.3 + 3.75)).abs() < 1e-9, "{cost}");
        // gpt-5: $1.25 in, $0.125 cached, $10 out.
        let cost = super::cost(Api::Openai, "gpt-5", &tokens).unwrap();
        assert!((cost - (1.25 + 0.125 + 1.0)).abs() < 1e-9, "{cost}");
        assert_eq!(super::cost(Api::Anthropic, "mystery-1", &tokens), None);
        assert!(super::cost(Api::Generic, "gpt-5", &tokens).is_some());
    }
}
