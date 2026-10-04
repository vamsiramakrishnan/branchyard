//! `by services [--kind K] [--all] [gc]`: the services this repository's
//! registry holds (`.branchyard/registry.db`), or a server's fleet with
//! `--remote`, with health, lease and owner; `gc` expires and reclaims
//! what stopped owners left. See docs/registry.md.

use branchyard::services::{Service, ServiceState};
use branchyard_support::time::now_ms;
use serde_json::json;

use crate::args::ServicesAction;
use crate::commands::{self, print, Env, Outcome, Target};
use crate::json;
use crate::render::{table, Cell, Column, Tone};

pub fn main(
    env: &Env,
    target: &Target,
    action: Option<&ServicesAction>,
    kind: Option<&str>,
    all: bool,
    as_json: bool,
) -> Outcome {
    let now = now_ms();
    match action {
        Some(ServicesAction::Gc) => {
            let reaped: Vec<serde_json::Value> = match target {
                Target::Local => {
                    // Opening the repository recovers it, which reaps too:
                    // what it reaped is reported with what gc does.
                    let yard = commands::open()?;
                    let mut reaped: Vec<serde_json::Value> = yard
                        .reclaim_services()?
                        .into_iter()
                        .map(|r| {
                            let (outcome, said) = match &r.outcome {
                                branchyard::services::Outcome::Done(s) => ("reclaimed", s.clone()),
                                branchyard::services::Outcome::Skipped(s) => ("skipped", s.clone()),
                                branchyard::services::Outcome::Failed(s) => ("failed", s.clone()),
                            };
                            json!({"id": r.service.id, "kind": r.service.kind,
                                   "outcome": outcome, "detail": said})
                        })
                        .collect();
                    if yard.has_services() {
                        for s in branchyard::services::ServiceStore::all(&*yard.services()?)? {
                            let outcome = match s.state {
                                ServiceState::Reclaimed => "reclaimed",
                                ServiceState::Expired => "failed",
                                _ => continue,
                            };
                            if s.changed_ms >= now && !reaped.iter().any(|r| r["id"] == s.id) {
                                reaped.push(json!({"id": s.id, "kind": s.kind,
                                    "outcome": outcome, "detail": s.note}));
                            }
                        }
                    }
                    reaped
                }
                Target::Remote(remote) => remote
                    .client
                    .reclaim_services()?
                    .into_iter()
                    .map(|s| {
                        json!({"id": s.id, "kind": s.kind, "outcome": s.state.as_str(),
                               "detail": s.note})
                    })
                    .collect(),
            };
            if as_json {
                return print(&json::text(&json!({ "reaped": reaped })));
            }
            if reaped.is_empty() {
                return print("nothing to reclaim\n");
            }
            let mut out = String::new();
            for r in &reaped {
                out.push_str(&format!(
                    "{} {} {}: {}\n",
                    r["outcome"].as_str().unwrap_or(""),
                    r["kind"].as_str().unwrap_or(""),
                    r["id"].as_str().unwrap_or(""),
                    r["detail"].as_str().unwrap_or("")
                ));
            }
            print(&out)
        }
        None => {
            let mut services = match target {
                Target::Local => {
                    let yard = commands::open()?;
                    match yard.has_services() {
                        true => branchyard::services::ServiceStore::all(&*yard.services()?)?,
                        false => Vec::new(),
                    }
                }
                Target::Remote(remote) => remote.client.services(kind)?,
            };
            services.retain(|s| kind.is_none_or(|k| k == s.kind) && (all || !s.state.ended()));
            services.sort_by(|a, b| a.kind.cmp(&b.kind).then(a.id.cmp(&b.id)));
            if as_json {
                return print(&json::text(&json!({ "services": services })));
            }
            if services.is_empty() {
                return print(match target {
                    Target::Local => {
                        "no services are registered in this repository (docs/registry.md)\n"
                    }
                    Target::Remote(_) => "the server's registry holds no services\n",
                });
            }
            print(&render(env, &services, now))
        }
    }
}

/// How long is left of a lease, or how long ago it ran out.
fn lease(service: &Service, now: u64) -> String {
    match service.lease_until_ms.checked_sub(now) {
        Some(left) if service.state == ServiceState::Live => format!("{}s", left / 1000),
        _ if service.state == ServiceState::Live => "ran out".into(),
        _ => "-".into(),
    }
}

fn owner(service: &Service) -> String {
    let o = &service.owner;
    let mut parts = Vec::new();
    if o.pid != 0 {
        parts.push(format!("pid {}", o.pid));
    }
    if let Some(branch) = &o.branch {
        parts.push(format!("branch {branch}"));
    }
    if let Some(principal) = &o.principal {
        parts.push(principal.clone());
    }
    match parts.is_empty() {
        true => "-".into(),
        false => parts.join(", "),
    }
}

#[allow(clippy::map_unwrap_or)] // ratchet: branchyard-cli
fn render(env: &Env, services: &[Service], now: u64) -> String {
    let columns = [
        Column {
            header: "KIND",
            max: 18,
            right: false,
        },
        Column {
            header: "ID",
            max: 32,
            right: false,
        },
        Column {
            header: "STATE",
            max: 10,
            right: false,
        },
        Column {
            header: "HEALTH",
            max: 9,
            right: false,
        },
        Column {
            header: "LEASE",
            max: 8,
            right: true,
        },
        Column {
            header: "OWNER",
            max: 28,
            right: false,
        },
        Column {
            header: "ENDPOINT",
            max: 40,
            right: false,
        },
        Column {
            header: "CAPABILITIES",
            max: 60,
            right: false,
        },
    ];
    let rows: Vec<Vec<Cell>> = services
        .iter()
        .map(|s| {
            let state = match (s.state, s.live_at(now)) {
                (ServiceState::Live, true) => Cell::toned("live", Tone::Green),
                (ServiceState::Live, false) => Cell::toned("lapsed", Tone::Yellow),
                (ServiceState::Expired | ServiceState::Reclaiming, _) => {
                    Cell::toned(s.state.as_str(), Tone::Red)
                }
                (other, _) => Cell::toned(other.as_str(), Tone::Dim),
            };
            vec![
                Cell::plain(&s.kind),
                Cell::plain(&s.id),
                state,
                Cell::plain(s.health.as_str()),
                Cell::plain(lease(s, now)),
                Cell::plain(owner(s)),
                Cell::plain(
                    s.endpoints
                        .first()
                        .map(|e| e.describe())
                        .unwrap_or_else(|| "-".into()),
                ),
                Cell::plain(
                    s.capabilities
                        .iter()
                        .map(|(k, v)| format!("{k}={}", v.describe()))
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
            ]
        })
        .collect();
    let mut text = table(&columns, &rows, env.style());
    let noted: Vec<String> = services
        .iter()
        .filter_map(|s| s.note.as_ref().map(|n| format!("{}: {n}", s.id)))
        .collect();
    if !noted.is_empty() {
        text.push('\n');
        for line in noted {
            text.push_str(&line);
            text.push('\n');
        }
    }
    text
}
