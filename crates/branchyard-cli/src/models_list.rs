//! `by models list`: the models each of your keys can call, asked of the
//! providers' own APIs instead of read from the static lists in
//! `catalog/harnesses.toml` (docs/model-gateway.md, "Listing models").
//!
//! The discovery itself is `branchyard::models::discover`. Here: which
//! sources this repository's `[models]` and this machine's keys give, the
//! cache (beside the live catalogs, with its time), the pricing cross-check
//! against `catalog/pricing.toml` and `[models.prices]`, the `--harness`
//! comparison, and the text and JSON. A key is never printed.

use std::path::Path;
use std::time::Duration;

use branchyard::doctor::{price_source, PriceSource};
use branchyard::models::discover::{self, Bases, Comparison, Resolved, Standing, Status};
use branchyard::models::{Config, Transport};
use branchyard_controls::catalog::{self, HarnessEntry};
use serde_json::{json, Value};

use crate::args::ModelsAction;
use crate::commands::{print, Env, Failure, Outcome, Target};
use crate::render::{self, Cell, Column, Style, Tone};
use crate::{live_catalog, models_cmd, setup_io};

/// How long one read from a provider may wait.
const WAIT: Duration = Duration::from_secs(30);
/// The live models a harness view names that its catalog list does not, at
/// most (`--json` has them all).
const NEW_SHOWN: usize = 30;

/// What prices a listed model, as a label.
fn price_label(source: &PriceSource) -> String {
    match source {
        PriceSource::Catalog => "catalog".to_owned(),
        PriceSource::Configured(pattern) => format!("[models.prices] {pattern}"),
    }
}

/// How long ago `at_ms` was, in seconds.
fn age(now_ms: u64, at_ms: u64) -> u64 {
    now_ms.saturating_sub(at_ms) / 1000
}

/// The harness `--harness` names, and the built-in sources its keys are.
struct Focus {
    entry: &'static HarnessEntry,
    drives: bool,
    sources: Vec<&'static str>,
}

fn focus(id: &str) -> Result<Focus, Failure> {
    let entry = catalog::harnesses()
        .iter()
        .find(|h| h.id == id)
        .ok_or_else(|| {
            Failure::Message(format!(
                "no harness {id:?} in the catalog; `by harnesses --all` lists them"
            ))
        })?;
    Ok(Focus {
        entry,
        drives: branchyard_harness::profiles::PROFILES
            .iter()
            .any(|p| p.harness == entry.id),
        sources: discover::harness_sources(&entry.id, &entry.auth_env),
    })
}

/// `by models list [--refresh] [--harness ID] [--json]`.
pub fn main(env: &Env, target: &Target, action: &ModelsAction, as_json: bool) -> Outcome {
    let ModelsAction::List {
        refresh,
        harness,
        gemini_url,
        openai_url,
        anthropic_url,
    } = action;
    if let Target::Remote(_) = target {
        return Err(Failure::Message(
            "by models list asks the providers with the keys of this machine; a server's keys \
             are in its own environment (docs/model-gateway.md)"
                .into(),
        ));
    }
    if models_cmd::inside_branch() {
        return Err(Failure::Message(
            "by models list uses the keys in your environment; inside a branch the environment \
             holds the turn's token, which is not a provider's to see. Run it outside the harness"
                .into(),
        ));
    }
    let cwd = std::env::current_dir()?;
    let root = setup_io::project_root(&cwd);
    let project = models_cmd::config(&root)?;
    let (engine, secrets) = match &project {
        Some(project) => {
            let engine = branchyard::configure::models_config(&project.models)?;
            (engine, project.secrets.clone())
        }
        None => (Config::default(), Default::default()),
    };
    let focus = harness.as_deref().map(focus).transpose()?;
    let bases = Bases {
        gemini: gemini_url.clone(),
        openai: openai_url.clone(),
        anthropic: anthropic_url.clone(),
        zai: discover::ZAI_URL.to_owned(),
    };
    let mut sources = discover::sources(&engine, &bases, |name| {
        models_cmd::key_source(&root, &secrets, name)
    });
    if let Some(focus) = &focus {
        sources.retain(|s| focus.sources.contains(&s.id.as_str()));
    }

    let dir = live_catalog::dir();
    let cache = match live_catalog::load_models(&dir) {
        Ok(Some((_, listings))) => listings,
        Ok(None) => Vec::new(),
        Err(why) => {
            eprintln!("by: the cached model listings were refused: {why}");
            Vec::new()
        }
    };
    let now = branchyard_support::time::now_ms();
    let resolved = discover::resolve(&sources, &cache, *refresh, &Transport::from_env(WAIT), now);
    let fetched = resolved
        .iter()
        .any(|r| !r.from_cache && r.listing.status == Status::Listed);
    if fetched {
        if let Err(why) =
            live_catalog::store_models(&dir, &discover::merged(&cache, &resolved), now)
        {
            eprintln!("by: could not cache the model listings: {why}");
        }
    }

    let view = View {
        resolved: &resolved,
        engine: &engine,
        focus: focus.as_ref(),
        now,
        dir: &dir,
        fetched,
    };
    match as_json {
        true => print(&crate::json::text(&view.json())),
        false => print(&view.text(env.style())),
    }
}

/// Everything one run found, to show.
struct View<'a> {
    resolved: &'a [Resolved],
    engine: &'a Config,
    focus: Option<&'a Focus>,
    now: u64,
    dir: &'a Path,
    /// Some source was asked now, not read from the cache.
    fetched: bool,
}

impl View<'_> {
    /// What prices each of a source's models.
    fn prices<'r>(&self, resolved: &'r Resolved) -> Vec<(&'r str, Option<PriceSource>)> {
        resolved
            .listing
            .models
            .iter()
            .map(|model| {
                (
                    model.as_str(),
                    price_source(self.engine, resolved.source.api(), model),
                )
            })
            .collect()
    }

    /// How many of the listed models nothing prices.
    fn unpriced(&self) -> usize {
        self.resolved
            .iter()
            .map(|r| self.prices(r).iter().filter(|(_, p)| p.is_none()).count())
            .sum()
    }

    fn total(&self) -> usize {
        self.resolved.iter().map(|r| r.listing.models.len()).sum()
    }

    fn source_json(&self, resolved: &Resolved) -> Value {
        let listing = &resolved.listing;
        let listed = listing.status == Status::Listed;
        let prices = self.prices(resolved);
        let mut value = json!({
            "source": listing.source,
            "kind": listing.kind,
            "url": listing.url,
            "backend": resolved.source.backend,
            "status": listing.status,
            "reason": listing.reason,
            "key": listing.key,
            "count": listing.models.len(),
            "unpriced": prices.iter().filter(|(_, p)| p.is_none()).count(),
            "models": prices
                .iter()
                .map(|(name, price)| json!({
                    "name": name,
                    "priced": price.is_some(),
                    "price": price.as_ref().map(price_label),
                }))
                .collect::<Vec<_>>(),
        });
        if listed {
            value["cached"] = json!(resolved.from_cache);
            value["fetched_at_ms"] = json!(listing.fetched_at_ms);
            value["age_s"] = json!(age(self.now, listing.fetched_at_ms));
        }
        if let Some(url) = resolved.source.openai_compatible() {
            value["openai_compatible"] = json!({
                "url": url,
                "api": "chat_completions",
                "rejects": ["store"],
            });
        }
        value
    }

    fn json(&self) -> Value {
        let mut value = json!({
            "cache": {"dir": self.dir, "refreshed": self.fetched},
            "sources": self.resolved.iter().map(|r| self.source_json(r)).collect::<Vec<_>>(),
            "models": self.total(),
            "unpriced": self.unpriced(),
        });
        if let Some(focus) = self.focus {
            let sources: Vec<Value> = self
                .resolved
                .iter()
                .map(|r| {
                    let comparison = (r.listing.status == Status::Listed)
                        .then(|| discover::compare(&focus.entry.models, &r.listing.models));
                    json!({
                        "source": r.listing.source,
                        "status": r.listing.status,
                        "reason": r.listing.reason,
                        "comparison": comparison,
                    })
                })
                .collect();
            value["harness"] = json!({
                "id": focus.entry.id,
                "name": focus.entry.name,
                "drives": focus.drives,
                "auth_env": focus.entry.auth_env,
                "static_models": focus.entry.models,
                "sources": sources,
            });
        }
        value
    }

    /// One source's heading and, when it is listed, what it says.
    fn heading(&self, resolved: &Resolved, style: Style) -> String {
        let listing = &resolved.listing;
        let id = format!("{:<18}", listing.source);
        match listing.status {
            Status::Listed => {
                let when = match age(self.now, listing.fetched_at_ms) {
                    0..=9 => "fetched now".to_owned(),
                    seconds => format!("fetched {} ago", render::age_text(seconds)),
                };
                let cached = if resolved.from_cache { ", cached" } else { "" };
                format!(
                    "{}{}\n  {} models, key {}, {when}{cached}\n",
                    style.paint(Tone::Bold, &id),
                    style.paint(Tone::Dim, &listing.url),
                    listing.models.len(),
                    listing.key.as_deref().unwrap_or("-"),
                )
            }
            Status::Skipped => format!(
                "{}{}\n",
                style.paint(Tone::Bold, &id),
                style.paint(
                    Tone::Dim,
                    &format!("skipped: {}", listing.reason.as_deref().unwrap_or(""))
                ),
            ),
            Status::Failed => format!(
                "{}{} {}\n",
                style.paint(Tone::Bold, &id),
                style.paint(Tone::Red, "failed:"),
                listing.reason.as_deref().unwrap_or(""),
            ),
        }
    }

    fn text(&self, style: Style) -> String {
        if let Some(focus) = self.focus {
            return self.harness_text(focus, style);
        }
        let listed = self
            .resolved
            .iter()
            .filter(|r| r.listing.status == Status::Listed)
            .count();
        let mut out = String::new();
        if listed == 0 {
            out.push_str("No source listed models.");
            if self
                .resolved
                .iter()
                .all(|r| r.listing.status != Status::Failed)
            {
                out.push_str(
                    " A key in GEMINI_API_KEY, OPENAI_API_KEY (or CODEX_API_KEY) or \
                     ANTHROPIC_API_KEY, or one a [models.backends] entry names, lets one.",
                );
            }
            out.push('\n');
        }
        let mut after_models = false;
        for (i, resolved) in self.resolved.iter().enumerate() {
            let this_lists = resolved.listing.status == Status::Listed;
            if i > 0 && (after_models || this_lists) {
                out.push('\n');
            }
            after_models = this_lists;
            out.push_str(&self.heading(resolved, style));
            if !this_lists {
                continue;
            }
            if let Some(url) = resolved.source.openai_compatible() {
                out.push_str(&format!(
                    "  the same key serves the OpenAI chat completions API at {url} (it rejects \
                     the OpenAI-only \"store\" field)\n"
                ));
            }
            let columns = [
                Column {
                    header: "MODEL",
                    max: 64,
                    right: false,
                },
                Column {
                    header: "PRICE",
                    max: 40,
                    right: false,
                },
            ];
            let rows: Vec<Vec<Cell>> = self
                .prices(resolved)
                .into_iter()
                .map(|(name, price)| {
                    vec![
                        Cell::plain(name),
                        match price {
                            Some(source) => Cell::toned(price_label(&source), Tone::Dim),
                            None => Cell::toned("unpriced", Tone::Yellow),
                        },
                    ]
                })
                .collect();
            if rows.is_empty() {
                out.push_str("  no models\n");
                continue;
            }
            for line in render::table(&columns, &rows, style).lines() {
                out.push_str("  ");
                out.push_str(line);
                out.push('\n');
            }
        }
        if listed > 0 {
            let (total, unpriced) = (self.total(), self.unpriced());
            out.push_str(&format!(
                "\n{total} model{} from {listed} source{}",
                if total == 1 { "" } else { "s" },
                if listed == 1 { "" } else { "s" },
            ));
            if unpriced > 0 {
                out.push_str(&format!(
                    "; {unpriced} cannot be priced by catalog/pricing.toml or [models.prices], so \
                     a dollar budget does not count their calls"
                ));
            }
            out.push_str(&format!(
                ".\ncached in {} (by models list --refresh asks again)\n",
                self.dir.display()
            ));
        }
        out
    }

    /// `--harness`: the catalog's static list beside what the live source
    /// of each of its keys says.
    fn harness_text(&self, focus: &Focus, style: Style) -> String {
        let entry = focus.entry;
        let mut out = format!(
            "{} ({}): {}\n",
            style.paint(Tone::Bold, &entry.name),
            entry.id,
            match focus.drives {
                true => "Branchyard drives it",
                false => "in the catalog; Branchyard has no profile to drive it",
            }
        );
        out.push_str(&format!(
            "  catalog/harnesses.toml lists {} model{}{}\n",
            entry.models.len(),
            if entry.models.len() == 1 { "" } else { "s" },
            match entry.auth_env.is_empty() {
                true => String::new(),
                false => format!("; its keys: {}", entry.auth_env.join(", ")),
            }
        ));
        if focus.sources.is_empty() {
            out.push_str(&format!(
                "  no live source: {} reads no API key whose provider this command lists models \
                 of, so its catalog list is all there is\n",
                entry.id
            ));
        }
        for resolved in self.resolved {
            out.push('\n');
            out.push_str(&self.heading(resolved, style));
            if resolved.listing.status != Status::Listed {
                continue;
            }
            let comparison = discover::compare(&entry.models, &resolved.listing.models);
            out.push_str(&comparison_text(&comparison, style));
        }
        out
    }
}

/// A harness's static names with where each stands against the live list,
/// then the live models its list does not name.
fn comparison_text(comparison: &Comparison, style: Style) -> String {
    let mut out = String::new();
    if comparison.static_models.is_empty() {
        out.push_str("  the catalog lists no models for it\n");
    } else {
        let columns = [
            Column {
                header: "CATALOG LIST",
                max: 48,
                right: false,
            },
            Column {
                header: "LIVE",
                max: 60,
                right: false,
            },
        ];
        let rows: Vec<Vec<Cell>> = comparison
            .static_models
            .iter()
            .map(|m| {
                vec![
                    Cell::plain(&m.name),
                    match m.standing {
                        Standing::Listed => Cell::toned("listed", Tone::Green),
                        Standing::Alias => {
                            Cell::toned(format!("an alias: {}", m.live.join(", ")), Tone::Dim)
                        }
                        Standing::NotListed => Cell::toned("not listed", Tone::Yellow),
                    },
                ]
            })
            .collect();
        for line in render::table(&columns, &rows, style).lines() {
            out.push_str("  ");
            out.push_str(line);
            out.push('\n');
        }
    }
    if !comparison.only_live.is_empty() {
        let shown: Vec<&str> = comparison
            .only_live
            .iter()
            .take(NEW_SHOWN)
            .map(String::as_str)
            .collect();
        let more = comparison.only_live.len().saturating_sub(NEW_SHOWN);
        out.push_str(&format!(
            "  {} live model{} the catalog list does not name: {}{}\n",
            comparison.only_live.len(),
            if comparison.only_live.len() == 1 {
                ""
            } else {
                "s"
            },
            shown.join(", "),
            match more {
                0 => String::new(),
                more => format!(" (and {more} more; --json has them all)"),
            }
        ));
    }
    out
}
