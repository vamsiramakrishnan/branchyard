//! `by models`, and the configuration that gives the yard its model
//! gateway. See docs/model-gateway.md.
//!
//! `[models]` in branchyard.toml names the backends (their API, base URL
//! and the secret holding each key), the routes by model, the budgets and
//! any prices the catalog lacks. A backend's `key` names an entry of
//! `[secrets]` (whose value says the variable or file that holds it), or
//! else a variable of that name; the key is read when a turn's gateway
//! starts, in this process, and never given to a harness.

use std::collections::BTreeMap;
use std::path::Path;

use branchyard::models::{self, Config, Gateway, KeySource, ModelActivity, Signer, UsageRecord};
use branchyard::{Activity, RecordedEvent, Yard};
use branchyard_setup::config::ProjectConfig;
use serde_json::{json, Value};

use crate::commands::{open, print, Env, Failure, Outcome, Target};
use crate::{render, setup_io};

/// The configuration of the yard at `root`: `None` without a file, or
/// inside a harness on a branch (whose `by` acts through its engine).
fn config(root: &Path) -> Result<Option<ProjectConfig>, Failure> {
    if std::env::var_os("BRANCHYARD_BRANCH").is_some_and(|v| !v.is_empty()) {
        return Ok(None);
    }
    let located = setup_io::locate(root);
    if !located.project_exists && !located.user.is_file() {
        return Ok(None);
    }
    let effective = setup_io::load(root, None).map_err(|e| {
        Failure::Message(format!(
            "{e}\n(fix it, or check it with `by config validate`)"
        ))
    })?;
    Ok(Some(effective.config))
}

/// Where the secret `name` is: its `[secrets]` entry's variable or file
/// (relative to `root`, or `~/`), else the variable `name`.
pub fn key_source(root: &Path, secrets: &BTreeMap<String, String>, name: &str) -> KeySource {
    match secrets.get(name) {
        Some(source) => match source.strip_prefix('@') {
            Some(path) => KeySource::File(setup_io::resolve(root, path)),
            None => KeySource::Env(source.clone()),
        },
        None => KeySource::Env(name.to_owned()),
    }
}

/// The gateway `config`'s `[models]` describes for `yard`, signing with the
/// connector gateway's keys when the yard has one; `None` when it names no
/// backend.
fn gateway(yard: &Yard, config: &ProjectConfig) -> Result<Option<Gateway>, Failure> {
    if config.models.backends.is_empty() {
        return Ok(None);
    }
    let engine: Config = serde_json::to_value(&config.models)
        .and_then(serde_json::from_value)
        .map_err(|e| Failure::Message(format!("[models]: {e}")))?;
    let mut signer = Signer::local(yard)?;
    if let Some(connectors) = yard.connectors() {
        signer.issuer = connectors.issuer.clone();
        signer.key_file = connectors.key_file.clone();
        signer.jwks_file = connectors.jwks_file.clone();
    }
    let gateway = Gateway::new(&engine, signer, |name| {
        key_source(yard.root(), &config.secrets, name)
    })
    .map_err(|e| Failure::Message(format!("[models]: {e}")))?;
    Ok(Some(gateway))
}

/// Give `yard` the model gateway `[models]` configures, if any.
pub fn configure(yard: &Yard) -> Result<(), Failure> {
    if let Some(config) = config(yard.root())? {
        if let Some(gateway) = gateway(yard, &config)? {
            yard.use_models(gateway);
        }
    }
    Ok(())
}

/// `by models`: the routes, backends and budgets, and the yard's usage
/// over `period`.
pub fn main(env: &Env, target: &Target, period: &str, as_json: bool) -> Outcome {
    if let Target::Remote(_) = target {
        return Err(Failure::Message(
            "by models reads this repository's [models] and usage; a server's are in its \
             configuration file and /metrics (docs/model-gateway.md)"
                .into(),
        ));
    }
    let yard = open()?;
    let gateway = yard.models();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let (day, month) = models::period_starts(now);
    let since = match period {
        "day" => day,
        "month" => month,
        _ => 0,
    };
    let rows = yard.model_usage(since)?;
    let (totals, by_model) = models::summarize(&rows);
    let spent = |start: u64| {
        let within: Vec<UsageRecord> = rows.iter().filter(|r| r.at_ms >= start).cloned().collect();
        models::summarize(&within).0
    };
    let (today, this_month) = match period {
        "day" => (totals.clone(), None),
        _ => (spent(day), Some(spent(month))),
    };
    let this_month = match this_month {
        Some(month) => month,
        None => models::summarize(&yard.model_usage(month)?).0,
    };
    if as_json {
        let value = json!({
            "configured": gateway.is_some(),
            "backends": gateway.as_ref().map(|g| backends_json(g)).unwrap_or_default(),
            "routes": gateway.as_ref().map(|g| routes_json(g)).unwrap_or_default(),
            "budget": gateway.as_ref().map(|g| budget_json(g, &today, &this_month)),
            "period": period,
            "since_ms": since,
            "usage": totals,
            "by_model": by_model,
        });
        return print(&crate::json::text(&value));
    }
    let style = env.style();
    let mut out = String::new();
    match &gateway {
        None => out.push_str(
            "No model gateway: [models] in branchyard.toml names no backend \
             (docs/model-gateway.md).\n",
        ),
        Some(gateway) => {
            out.push_str(&style.paint(render::Tone::Bold, "backends"));
            out.push('\n');
            for b in &gateway.backends {
                let key = match &b.key {
                    Some(source) => match source.read() {
                        Ok(_) => format!("key {source}"),
                        Err(why) => format!("key {source} ({why})"),
                    },
                    None => "no key".to_owned(),
                };
                out.push_str(&format!("  {:<14} {:<9} {}  {key}\n", b.name, b.api, b.url));
            }
            if !gateway.routes.is_empty() {
                out.push_str(&style.paint(render::Tone::Bold, "routes"));
                out.push('\n');
                for r in &gateway.routes {
                    out.push_str(&format!("  {:<14} {}\n", r.model, route_line(r)));
                }
            }
            let lines = budget_lines(gateway, &today, &this_month);
            if !lines.is_empty() {
                out.push_str(&style.paint(render::Tone::Bold, "budget"));
                out.push('\n');
                for line in lines {
                    out.push_str(&format!("  {line}\n"));
                }
            }
        }
    }
    let label = match period {
        "day" => "today",
        "month" => "this month",
        _ => "in all",
    };
    out.push_str(&style.paint(render::Tone::Bold, &format!("usage {label}")));
    out.push('\n');
    out.push_str(&format!("  {}\n", totals_line(&totals)));
    for (model, t) in &by_model {
        out.push_str(&format!("  {:<28} {}\n", model, totals_line(t)));
    }
    print(&out)
}

fn route_line(route: &models::Route) -> String {
    let weighted: Vec<String> = route
        .backends
        .iter()
        .map(|(name, weight)| match route.backends.len() {
            1 => name.clone(),
            _ => format!("{name} ({weight})"),
        })
        .collect();
    let mut line = weighted.join(", ");
    if !route.fallbacks.is_empty() {
        line.push_str(&format!("; then {}", route.fallbacks.join(", ")));
    }
    if let Some(rpm) = route.requests_per_minute {
        line.push_str(&format!("; {rpm} requests a minute"));
    }
    line
}

fn totals_line(t: &models::UsageTotals) -> String {
    let mut line = format!(
        "{} call{}, {} in / {} out tokens",
        t.calls,
        if t.calls == 1 { "" } else { "s" },
        render::tokens(t.input_tokens + t.cache_read_tokens + t.cache_write_tokens),
        render::tokens(t.output_tokens)
    );
    if t.cache_read_tokens > 0 || t.cache_write_tokens > 0 {
        line.push_str(&format!(
            " ({} cache read, {} cache write)",
            render::tokens(t.cache_read_tokens),
            render::tokens(t.cache_write_tokens)
        ));
    }
    line.push_str(&format!(", {}", render::usd(t.cost_usd)));
    if t.unpriced > 0 {
        line.push_str(&format!(" ({} unpriced)", t.unpriced));
    }
    line
}

fn budget_lines(
    gateway: &Gateway,
    today: &models::UsageTotals,
    month: &models::UsageTotals,
) -> Vec<String> {
    let b = &gateway.budget;
    let mut lines = Vec::new();
    let share = |used: f64, limit: f64| match limit > 0.0 {
        true => format!("{:.0}%", used / limit * 100.0),
        false => "100%".to_owned(),
    };
    if let Some(limit) = b.daily_usd {
        lines.push(format!(
            "daily {} of {} ({})",
            render::usd(today.cost_usd),
            render::usd(limit),
            share(today.cost_usd, limit)
        ));
    }
    if let Some(limit) = b.monthly_usd {
        lines.push(format!(
            "monthly {} of {} ({})",
            render::usd(month.cost_usd),
            render::usd(limit),
            share(month.cost_usd, limit)
        ));
    }
    if let Some(limit) = b.daily_tokens {
        lines.push(format!(
            "daily {} of {} tokens",
            render::tokens(today.tokens()),
            render::tokens(limit)
        ));
    }
    if let Some(limit) = b.monthly_tokens {
        lines.push(format!(
            "monthly {} of {} tokens",
            render::tokens(month.tokens()),
            render::tokens(limit)
        ));
    }
    if !lines.is_empty() {
        lines.push(format!(
            "alert at {:.0}%",
            b.alert_at.unwrap_or(0.8) * 100.0
        ));
    }
    lines
}

fn backends_json(gateway: &Gateway) -> Value {
    Value::Array(
        gateway
            .backends
            .iter()
            .map(|b| {
                json!({
                    "name": b.name,
                    "api": b.api,
                    "url": b.url,
                    "key": b.key.as_ref().map(ToString::to_string),
                    "key_set": b.key.as_ref().is_some_and(|k| k.read().is_ok()),
                })
            })
            .collect(),
    )
}

fn routes_json(gateway: &Gateway) -> Value {
    Value::Array(
        gateway
            .routes
            .iter()
            .map(|r| {
                json!({
                    "model": r.model,
                    "backends": r.backends.iter().map(|(n, w)| json!({"backend": n, "weight": w})).collect::<Vec<_>>(),
                    "fallbacks": r.fallbacks,
                    "requests_per_minute": r.requests_per_minute,
                })
            })
            .collect(),
    )
}

fn budget_json(
    gateway: &Gateway,
    today: &models::UsageTotals,
    month: &models::UsageTotals,
) -> Value {
    json!({
        "daily_usd": gateway.budget.daily_usd,
        "monthly_usd": gateway.budget.monthly_usd,
        "daily_tokens": gateway.budget.daily_tokens,
        "monthly_tokens": gateway.budget.monthly_tokens,
        "alert_at": gateway.budget.alert_at.unwrap_or(0.8),
        "today": today,
        "this_month": month,
    })
}

/// What a branch's model access came to, from its events: as JSON and as
/// one line for `by show`. `None` when it was never on the gateway.
pub fn summary(events: &[RecordedEvent]) -> Option<(Value, String)> {
    let mut mode: Option<&ModelActivity> = None;
    let mut totals = models::UsageTotals::default();
    let mut refused: BTreeMap<String, u64> = BTreeMap::new();
    for event in events {
        let Activity::Model(activity) = &event.activity else {
            continue;
        };
        match activity.as_ref() {
            ModelActivity::Gateway { .. } | ModelActivity::Direct { .. } => {
                mode = Some(activity.as_ref())
            }
            ModelActivity::Call(call) => match call.decision.as_str() {
                "allowed" | "failed" if call.backend.is_some() => {
                    totals.calls += 1;
                    if let Some(t) = &call.tokens {
                        totals.input_tokens += t.input;
                        totals.output_tokens += t.output;
                        totals.cache_read_tokens += t.cache_read;
                        totals.cache_write_tokens += t.cache_write + t.cache_write_1h;
                    }
                    match call.cost_usd {
                        Some(cost) => totals.cost_usd += cost,
                        None => totals.unpriced += 1,
                    }
                }
                other => *refused.entry(other.to_owned()).or_default() += 1,
            },
            ModelActivity::Alert { .. } => {}
        }
    }
    let mode = mode?;
    let (kind, detail) = match mode {
        ModelActivity::Gateway { models, .. } => ("gateway", models.join(", ")),
        ModelActivity::Direct { reason, .. } => ("direct", reason.clone()),
        _ => return None,
    };
    let mut text = format!("{kind} ({detail})");
    if kind == "gateway" || totals.calls > 0 {
        text.push_str(&format!("; {}", totals_line(&totals)));
    }
    if !refused.is_empty() {
        let parts: Vec<String> = refused.iter().map(|(k, n)| format!("{n} {k}")).collect();
        text.push_str(&format!("; refused {}", parts.join(", ")));
    }
    let value = json!({
        "mode": kind,
        "models": match mode { ModelActivity::Gateway { models, .. } => json!(models), _ => Value::Null },
        "reason": match mode { ModelActivity::Direct { reason, .. } => json!(reason), _ => Value::Null },
        "calls": totals.calls,
        "input_tokens": totals.input_tokens,
        "output_tokens": totals.output_tokens,
        "cache_read_tokens": totals.cache_read_tokens,
        "cache_write_tokens": totals.cache_write_tokens,
        "cost_usd": totals.cost_usd,
        "unpriced": totals.unpriced,
        "refused": refused,
    });
    Some((value, text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_found_through_secrets_else_by_variable() {
        let secrets: BTreeMap<String, String> = [
            ("anthropic".to_owned(), "MY_KEY".to_owned()),
            ("file".to_owned(), "@/run/key".to_owned()),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            key_source(Path::new("/r"), &secrets, "anthropic"),
            KeySource::Env("MY_KEY".into())
        );
        assert_eq!(
            key_source(Path::new("/r"), &secrets, "file"),
            KeySource::File("/run/key".into())
        );
        assert_eq!(
            key_source(Path::new("/r"), &secrets, "OPENAI_API_KEY"),
            KeySource::Env("OPENAI_API_KEY".into())
        );
    }
}
