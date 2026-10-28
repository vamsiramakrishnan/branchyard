//! `by models`. The configuration that gives the yard its model gateway
//! is the engine's (`branchyard::configure`). See docs/model-gateway.md.
//!
//! `[models]` in branchyard.toml names the backends (their API, base URL
//! and the secret holding each key), the routes by model, the budgets and
//! any prices the catalog lacks. A backend's `key` names an entry of
//! `[secrets]` (whose value says the variable or file that holds it), or
//! else a variable of that name; the key is read when a turn's gateway
//! starts, in this process, and never given to a harness.

use std::collections::BTreeMap;
use std::path::Path;

use branchyard::models::{self, Gateway, ModelActivity, UsageRecord};
use branchyard::{Activity, RecordedEvent};
use branchyard_setup::config::ProjectConfig;
use serde_json::{json, Value};

use crate::commands::{open, print, Env, Failure, Outcome, Target};
use crate::render;

/// Whether this process runs inside a harness on a branch, where its
/// environment holds the turn's token and `by` acts through its engine.
pub(crate) fn inside_branch() -> bool {
    std::env::var_os("BRANCHYARD_BRANCH").is_some_and(|v| !v.is_empty())
}

/// The configuration of the yard at `root`: `None` without a file, or
/// inside a harness on a branch (whose `by` acts through its engine).
pub(crate) fn config(root: &Path) -> Result<Option<ProjectConfig>, Failure> {
    if inside_branch() {
        return Ok(None);
    }
    Ok(branchyard::configure::configuration(root)?.map(|effective| effective.config))
}

/// Where the secret `name` is: its `[secrets]` entry's variable or file
/// (relative to `root`, or `~/`), else the variable `name`.
pub use branchyard::configure::key_source;

/// `by models`: the routes, backends and budgets, and the yard's usage
/// over `period`.
pub fn main(env: &Env, target: &Target, period: &str, as_json: bool) -> Outcome {
    if let Target::Remote(_) = target {
        return Err(crate::errors::Report::usage(
            "by models reads this repository's [models] and usage; a server's are in its \
             configuration file and /metrics (docs/model-gateway.md)",
        )
        .next("run by models without --remote, in the repository")
        .into());
    }
    let yard = open()?;
    let gateway = yard.models();
    let now = branchyard_support::time::now_ms();
    let (day, month) = branchyard_support::time::period_starts(now);
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
            "audit": gateway.as_ref().map(|g| g.audit.to_string()),
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
            out.push_str(&style.paint(render::Tone::Bold, "audit"));
            out.push('\n');
            out.push_str(&format!("  {}\n", audit_line(gateway.audit)));
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

fn audit_line(audit: models::Audit) -> &'static str {
    match audit {
        models::Audit::Digests => {
            "digests: sizes, a request digest, tools and stop reason per call"
        }
        models::Audit::Contents => {
            "contents: also each call's request and response, in .branchyard/models/audit/"
        }
    }
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
    if let Some(as_model) = &route.backend_model {
        line.push_str(&format!("; translated as {as_model}"));
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
                    "backend_model": r.backend_model,
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
    let mut tool_uses = 0u64;
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
                    if let Some(interception) = &call.interception {
                        tool_uses += interception.tool_uses.len() as u64;
                    }
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
        "tool_uses": tool_uses,
        "refused": refused,
    });
    Some((value, text))
}
