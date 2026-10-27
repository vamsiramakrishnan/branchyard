//! `hooks.ID`: one row for each harness on `PATH` whose Branchyard hooks are
//! installed (or were heard), saying whether a session of that harness was
//! heard running them, from the notes `by hook` and `by statusline` keep
//! ([`heard`]). A hook that only answers when `by setup` calls it proves
//! nothing about the harness loading it; a note does.

use std::path::Path;

use branchyard_setup::probe::harness_label;
use serde_json::{json, Value};

use super::context::Context;
use super::Check;
use crate::hook_cmd::heard::{self, Hearing, State};

/// One harness whose hooks may run.
pub(crate) struct Hooked {
    /// The harness id.
    pub harness: String,
    /// Its name, as people read it.
    pub label: String,
    /// Its `by init plugin` install: `current`, `stale`, `missing` or
    /// `conflict` (missing, with a session heard: its own plugin's hooks).
    pub install: &'static str,
    /// What was heard.
    pub hearing: Hearing,
}

/// The harnesses on `PATH`, from `base`, whose hooks are installed or were
/// heard, in catalog order.
pub(crate) fn hooked(base: &Path) -> Result<Vec<Hooked>, String> {
    let store = heard::store();
    Ok(crate::setup_cmd::installs(base, true)?
        .into_iter()
        .filter_map(|install| {
            let hearing = Hearing::of(&store, &install.manifest.harness);
            install.hooked(&hearing).then(|| Hooked {
                label: harness_label(&install.manifest.harness),
                harness: install.manifest.harness,
                install: install.state,
                hearing,
            })
        })
        .collect())
}

/// The rows: `ok` when heard since the hooks were last written, `todo`
/// when never heard or heard only before.
pub(super) fn heard(ctx: &Context) -> Vec<Check> {
    match ctx.hooks() {
        Ok(hooked) => {
            let by_on_path = crate::setup_io::which("by").is_some();
            let now = branchyard_support::time::now_ms();
            hooked.iter().map(|h| row(h, by_on_path, now)).collect()
        }
        Err(error) => vec![Check::warn(
            "hooks.summary",
            format!("the hook installs could not be read: {error}"),
            "run `by init plugin --status` to see why",
        )],
    }
}

/// One harness's row.
fn row(hooked: &Hooked, by_on_path: bool, now: u64) -> Check {
    let name = format!("hooks.{}", hooked.harness);
    let hearing = &hooked.hearing;
    let install = format!("install {}", hooked.install);
    let written = hearing.installed_at.map(|at| heard::ago(now, at));
    let fix = match by_on_path {
        false => "put `by` on PATH: every hook starts with `command -v by`, so none of them \
                  runs without it"
            .to_owned(),
        true => format!(
            "start a new {} session (in any repository), then run `by doctor` again; if it \
             stays unheard, `by init plugin --status {}` lists the one-time steps (a trust \
             prompt, a restart) that keep a hook from running",
            hooked.label, hooked.harness
        ),
    };
    match (hearing.state(), &hearing.last) {
        (State::Heard, Some((kind, call))) => {
            let place = call
                .repository
                .as_deref()
                .map(|repo| format!(" in {repo}"))
                .unwrap_or_default();
            let event = call
                .harness_event
                .as_deref()
                .map(|event| format!(", as {event}"))
                .unwrap_or_default();
            Check::ok(
                name,
                format!(
                    "{install} · heard {} {}{place}{event}",
                    heard::ago(now, call.at_ms),
                    hearing.what(kind)
                ),
            )
        }
        (State::Stale, Some((kind, call))) => Check::todo(
            name,
            format!(
                "{install} · last heard {} {}, before by setup rewrote the hooks {}",
                heard::ago(now, call.at_ms),
                hearing.what(kind),
                written.unwrap_or_default()
            ),
            fix,
        ),
        _ => Check::todo(
            name,
            match written {
                Some(ago) => {
                    format!("{install} · never heard since by setup wrote the hooks {ago}")
                }
                None => format!("{install} · never heard"),
            },
            fix,
        ),
    }
}

/// The `hooks` array `--json` prints beside the rows.
pub(super) fn json(ctx: &Context) -> Value {
    let now = branchyard_support::time::now_ms();
    match ctx.hooks() {
        Ok(hooked) => Value::Array(
            hooked
                .iter()
                .map(|h| {
                    json!({
                        "harness": h.harness,
                        "label": h.label,
                        "install": h.install,
                        "heard": h.hearing.json(now),
                    })
                })
                .collect(),
        ),
        Err(_) => Value::Array(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook_cmd::heard::Call;

    const NOW: u64 = 1_800_000_000_000;

    fn hooked(last: Option<Call>, installed_at: Option<u64>) -> Hooked {
        Hooked {
            harness: "codex".into(),
            label: "Codex".into(),
            install: "current",
            hearing: Hearing {
                harness: "codex".into(),
                dialect: "codex".into(),
                last: last.map(|call| ("session-start".to_owned(), call)),
                installed_at,
            },
        }
    }

    #[test]
    fn a_heard_hook_is_ok_and_names_when_where_and_as_what() {
        let call = Call {
            at_ms: NOW - 180_000,
            harness_event: Some("SessionStart".into()),
            session: Some("0123456789ab".into()),
            repository: Some("/home/me/app".into()),
        };
        let check = row(&hooked(Some(call), Some(NOW - 600_000)), true, NOW);
        assert_eq!(check.name, "hooks.codex");
        assert_eq!(check.status, super::super::Status::Ok);
        assert_eq!(
            check.detail,
            "install current · heard 3m ago (session-start) in /home/me/app, as SessionStart"
        );
        assert_eq!(check.fix, None);
    }

    #[test]
    fn never_heard_and_heard_only_before_the_install_are_steps_to_take() {
        let never = row(&hooked(None, Some(NOW - 7_200_000)), true, NOW);
        assert_eq!(never.status, super::super::Status::Todo);
        assert_eq!(
            never.detail,
            "install current · never heard since by setup wrote the hooks 2h ago"
        );
        let fix = never.fix.unwrap_or_default();
        assert!(fix.starts_with("start a new Codex session"), "{fix}");
        assert!(fix.contains("by init plugin --status codex"), "{fix}");

        let call = Call {
            at_ms: NOW - 86_400_000 * 2,
            ..Call::default()
        };
        let stale = row(&hooked(Some(call), Some(NOW - 60_000)), true, NOW);
        assert_eq!(stale.status, super::super::Status::Todo);
        assert_eq!(
            stale.detail,
            "install current · last heard 2d ago (session-start), before by setup rewrote the \
             hooks 1m ago"
        );

        // Without `by` on PATH no hook can run: that is the fix.
        let off = row(&hooked(None, None), false, NOW);
        assert_eq!(off.detail, "install current · never heard");
        assert!(off.fix.unwrap_or_default().starts_with("put `by` on PATH"));
    }
}
