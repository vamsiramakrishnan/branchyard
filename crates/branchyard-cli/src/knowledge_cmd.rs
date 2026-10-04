//! `by knowledge`: review, adopt, reject, edit, add, remove, distill and
//! export repository knowledge, locally or on a server, and the
//! `[knowledge]` settings a local yard runs with. See docs/knowledge.md.

use std::io::{self, BufRead, Write};
use std::path::Path;
use std::sync::Arc;

use branchyard::{
    Distilled, JudgeSpec, KnowledgeEdit, KnowledgeEntry, KnowledgeScope, KnowledgeSettings,
    KnowledgeStatus, NewKnowledge, TaskKind, TaskOptions, Yard,
};
use branchyard_client::knowledge_api::{KnowledgeAddRequest, KnowledgeEditRequest};

use crate::args::KnowledgeAction;
use crate::commands::{self, print, Env, Failure, Outcome, Target};
use crate::plan_cmd::{edit_text, person};
use crate::render::{self, Tone};
use crate::setup_io;

/// The `[knowledge]` table under `root`, or `None` without one (or inside
/// a harness's branch, which reads no configuration).
fn config(root: &Path) -> Result<Option<branchyard_setup::config::KnowledgeConfig>, Failure> {
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
    let knowledge = effective.config.knowledge;
    Ok((!knowledge.is_empty()).then_some(knowledge))
}

/// The distiller `[knowledge] distiller` names, as a judge.
fn distiller(
    config: &branchyard_setup::config::DistillerConfig,
) -> Result<Arc<dyn branchyard::Judge>, Failure> {
    let effort = config
        .effort
        .as_deref()
        .map(branchyard::Effort::parse)
        .transpose()
        .map_err(|e| Failure::Message(format!("knowledge.distiller.effort: {e}")))?;
    let command = config
        .command
        .as_deref()
        .map(branchyard_setup::config::split_words)
        .transpose()
        .map_err(|e| Failure::Message(format!("knowledge.distiller.command: {e}")))?;
    Ok(Arc::new(branchyard::HarnessJudge {
        spec: JudgeSpec {
            harness: config.harness.clone(),
            model: config.model.clone(),
            effort,
            command,
            rubric: None,
        },
        options: TaskOptions::default(),
    }))
}

/// The settings `[knowledge]` describes.
pub fn settings(
    config: &branchyard_setup::config::KnowledgeConfig,
) -> Result<KnowledgeSettings, Failure> {
    let defaults = KnowledgeSettings::default();
    let distill_on = match &config.distill_on {
        None => defaults.distill_on,
        Some(names) => names
            .iter()
            .map(|name| match name.as_str() {
                "merged" => Ok(branchyard::DistillTrigger::Merged),
                "judged_best" => Ok(branchyard::DistillTrigger::JudgedBest),
                "ready" => Ok(branchyard::DistillTrigger::Ready),
                other => Err(Failure::Message(format!(
                    "knowledge.distill_on: {other:?} is not merged, judged_best or ready"
                ))),
            })
            .collect::<Result<Vec<_>, _>>()?,
    };
    Ok(KnowledgeSettings {
        provision: config.provision.unwrap_or(defaults.provision),
        budget_tokens: config
            .budget_tokens
            .map_or(defaults.budget_tokens, |t| t as usize),
        distill_on,
        distiller: config.distiller.as_ref().map(distiller).transpose()?,
    })
}

/// Give `yard` the settings `[knowledge]` configures, if any.
pub fn configure(yard: &Yard) -> Result<(), Failure> {
    if let Some(config) = config(yard.root())? {
        yard.use_knowledge(settings(&config)?);
    }
    Ok(())
}

fn status_tone(status: KnowledgeStatus) -> Tone {
    match status {
        KnowledgeStatus::Proposed => Tone::Yellow,
        KnowledgeStatus::Adopted => Tone::Green,
        KnowledgeStatus::Rejected => Tone::Dim,
    }
}

/// One entry, as `by knowledge show` prints it.
pub fn render_entry(entry: &KnowledgeEntry, style: render::Style) -> String {
    let mut pairs = vec![
        ("id", format!("k{}", entry.id)),
        (
            "status",
            style.paint(status_tone(entry.status), entry.status.as_str()),
        ),
        ("scope", entry.scope.describe()),
        ("source", entry.source.describe()),
    ];
    if let Some(by) = &entry.adopted_by {
        pairs.push(("adopted by", by.clone()));
    }
    if let Some(note) = &entry.note {
        pairs.push(("note", note.clone()));
    }
    pairs.push(("text", entry.text.clone()));
    render::key_values(&pairs, style)
}

/// Entries as a table.
pub fn render_list(entries: &[KnowledgeEntry], style: render::Style) -> String {
    if entries.is_empty() {
        return "no knowledge entries\n".into();
    }
    let mut out = String::new();
    for entry in entries {
        let text: String = entry.text.split_whitespace().collect::<Vec<_>>().join(" ");
        let text = match text.chars().count() > 90 {
            true => format!("{}…", text.chars().take(89).collect::<String>()),
            false => text,
        };
        out.push_str(&format!(
            "{:>5}  {:<9}  {:<24}  {text}\n",
            format!("k{}", entry.id),
            style.paint(status_tone(entry.status), entry.status.as_str()),
            entry.scope.describe(),
        ));
    }
    out
}

fn distilled_text(distilled: &Distilled) -> String {
    let mut text = format!(
        "distilled {} ({}): {} proposed",
        distilled.branch,
        distilled.by.describe(),
        distilled.proposed.len()
    );
    if distilled.duplicates > 0 {
        text.push_str(&format!(", {} already known", distilled.duplicates));
    }
    if let branchyard::JudgedBy::Fallback { error, .. } = &distilled.by {
        text.push_str(&format!("\nthe distiller's answer was not used: {error}"));
    }
    text.push('\n');
    for entry in &distilled.proposed {
        text.push_str(&format!(
            "  k{} ({}) {}\n",
            entry.id,
            entry.scope.describe(),
            entry.text
        ));
    }
    if !distilled.proposed.is_empty() {
        text.push_str("review them with: by knowledge review\n");
    }
    text
}

fn kind_arg(text: &Option<String>) -> Result<Option<Option<TaskKind>>, Failure> {
    match text.as_deref() {
        None => Ok(None),
        Some("") => Ok(Some(None)),
        Some(kind) => Ok(Some(Some(kind.parse()?))),
    }
}

fn path_arg(text: &Option<String>) -> Option<Option<String>> {
    text.as_ref()
        .map(|p| Some(p.clone()).filter(|p| !p.trim().is_empty()))
}

/// A storage-agnostic view: the local yard or a server's repository.
enum Store<'a> {
    Local(Yard),
    Remote(&'a branchyard_client::Repo),
}

impl Store<'_> {
    fn list(&self, status: Option<KnowledgeStatus>) -> Result<Vec<KnowledgeEntry>, Failure> {
        Ok(match self {
            Store::Local(yard) => yard.knowledge(status)?,
            Store::Remote(repo) => repo.knowledge(status)?,
        })
    }

    fn get(&self, id: u64) -> Result<KnowledgeEntry, Failure> {
        Ok(match self {
            Store::Local(yard) => yard.knowledge_entry(id)?,
            Store::Remote(repo) => repo.knowledge_entry(id)?,
        })
    }

    fn adopt(&self, id: u64) -> Result<KnowledgeEntry, Failure> {
        Ok(match self {
            Store::Local(yard) => yard.adopt_knowledge(id, &person())?,
            Store::Remote(repo) => repo.adopt_knowledge(id)?,
        })
    }

    fn reject(&self, id: u64, reason: Option<&str>) -> Result<KnowledgeEntry, Failure> {
        Ok(match self {
            Store::Local(yard) => yard.reject_knowledge(id, &person(), reason)?,
            Store::Remote(repo) => repo.reject_knowledge(id, reason)?,
        })
    }

    fn edit(&self, id: u64, change: &KnowledgeEditRequest) -> Result<KnowledgeEntry, Failure> {
        Ok(match self {
            Store::Local(yard) => yard.edit_knowledge(
                id,
                &KnowledgeEdit {
                    text: change.text.clone(),
                    path: path_arg(&change.path),
                    kind: kind_arg(&change.kind)?,
                },
                &person(),
            )?,
            Store::Remote(repo) => repo.edit_knowledge(id, change)?,
        })
    }
}

pub fn main(env: &Env, target: &Target, action: &KnowledgeAction, json: bool) -> Outcome {
    if std::env::var_os(branchyard::ENV_BRANCH).is_some_and(|v| !v.is_empty()) {
        return commands::fail(
            json,
            &branchyard::Error::Denied(
                "by knowledge is for the people who decide what agents are told; a harness \
                 cannot adopt or change knowledge"
                    .into(),
            ),
        );
    }
    let result = run(env, target, action, json);
    match result {
        Err(Failure::Sdk(error)) => commands::fail(json, &error),
        other => other,
    }
}

fn emit<T: serde::Serialize>(json: bool, value: &T, text: impl FnOnce() -> String) -> Outcome {
    match json {
        true => print(&format!("{}\n", commands::to_json(value))),
        false => print(&text()),
    }
}

fn run(env: &Env, target: &Target, action: &KnowledgeAction, json: bool) -> Outcome {
    let style = env.style();
    let store = match target {
        Target::Local => Store::Local(commands::open()?),
        Target::Remote(remote) => Store::Remote(&remote.repo),
    };
    match action {
        KnowledgeAction::List { status, all } => {
            let mut entries = store.list(*status)?;
            if status.is_none() && !all {
                entries.retain(|e| e.status != KnowledgeStatus::Rejected);
            }
            emit(json, &entries, || render_list(&entries, style))
        }
        KnowledgeAction::Show { id } => {
            let entry = store.get(*id)?;
            emit(json, &entry, || render_entry(&entry, style))
        }
        KnowledgeAction::Adopt { ids } => {
            let mut done = Vec::new();
            for id in ids {
                done.push(store.adopt(*id)?);
            }
            emit(json, &done, || {
                done.iter()
                    .map(|e| format!("adopted k{}: {}\n", e.id, e.text))
                    .collect()
            })
        }
        KnowledgeAction::Reject { ids, reason } => {
            let mut done = Vec::new();
            for id in ids {
                done.push(store.reject(*id, reason.as_deref())?);
            }
            emit(json, &done, || {
                done.iter()
                    .map(|e| format!("rejected k{}: {}\n", e.id, e.text))
                    .collect()
            })
        }
        KnowledgeAction::Edit {
            id,
            text,
            path,
            kind,
            editor,
        } => {
            let text = match (text, path.is_none() && kind.is_none()) {
                (Some(text), _) => Some(text.clone()),
                // Nothing else to change: edit the text in the editor.
                (None, true) => {
                    let current = store.get(*id)?;
                    Some(edit_text(
                        &current.text,
                        editor.as_deref(),
                        &format!("knowledge-{id}"),
                    )?)
                }
                (None, false) => None,
            };
            let entry = store.edit(
                *id,
                &KnowledgeEditRequest {
                    text,
                    path: path.clone(),
                    kind: kind.clone(),
                },
            )?;
            emit(json, &entry, || render_entry(&entry, style))
        }
        KnowledgeAction::Add {
            text,
            path,
            kind,
            propose,
        } => {
            let scope = KnowledgeScope {
                path: path.clone().filter(|p| !p.trim().is_empty()),
                kind: *kind,
            };
            let entry = match &store {
                Store::Local(yard) => yard.add_knowledge(
                    &NewKnowledge {
                        text: text.clone(),
                        scope,
                        propose: *propose,
                        note: None,
                    },
                    &person(),
                )?,
                Store::Remote(repo) => repo.add_knowledge(&KnowledgeAddRequest {
                    text: text.clone(),
                    scope,
                    propose: *propose,
                })?,
            };
            emit(json, &entry, || {
                format!("{} k{}: {}\n", entry.status, entry.id, entry.text)
            })
        }
        KnowledgeAction::Rm { id } => {
            let entry = match &store {
                Store::Local(yard) => yard.remove_knowledge(*id)?,
                Store::Remote(repo) => repo.remove_knowledge(*id)?,
            };
            emit(json, &entry, || format!("removed k{}\n", entry.id))
        }
        KnowledgeAction::Export { out } => {
            let entries = store.list(Some(KnowledgeStatus::Adopted))?;
            let markdown = branchyard::export_knowledge(&entries);
            match out {
                Some(path) => {
                    std::fs::write(path, &markdown)?;
                    let ids: Vec<u64> = entries.iter().map(|e| e.id).collect();
                    emit(
                        json,
                        &serde_json::json!({"path": path, "entries": ids}),
                        || {
                            format!(
                                "wrote {} adopted entr{} to {path}\n",
                                ids.len(),
                                if ids.len() == 1 { "y" } else { "ies" }
                            )
                        },
                    )
                }
                None if json => emit(
                    true,
                    &serde_json::json!({"markdown": markdown}),
                    String::new,
                ),
                None => print(&markdown),
            }
        }
        KnowledgeAction::Distill {
            branch,
            harness,
            command,
            deterministic,
        } => {
            let distilled = match &store {
                Store::Local(yard) => {
                    let distiller: Option<Arc<dyn branchyard::Judge>> =
                        match (harness, deterministic) {
                            (_, true) => None,
                            (Some(harness), false) => Some(Arc::new(branchyard::HarnessJudge {
                                spec: JudgeSpec {
                                    harness: harness.clone(),
                                    model: None,
                                    effort: None,
                                    command: command.as_ref().map(|c| c.0.clone()),
                                    rubric: None,
                                },
                                options: TaskOptions::default(),
                            })),
                            (None, false) => yard.knowledge_settings().distiller.clone(),
                        };
                    yard.distill(branch, distiller)?
                }
                Store::Remote(repo) => {
                    if harness.is_some() {
                        return Err(Failure::Message(
                            "a server distills with the deterministic extractor only; drop \
                             --harness, or run by knowledge distill on the server's host"
                                .into(),
                        ));
                    }
                    repo.distill(branch)?
                }
            };
            emit(json, &distilled, || distilled_text(&distilled))
        }
        KnowledgeAction::Review { editor } => review(&store, editor.as_deref(), json, style),
    }
}

/// Walk the proposed entries: adopt, reject, edit or skip each, reading
/// one answer a line from stdin.
#[allow(clippy::let_underscore_must_use)] // ratchet: branchyard-cli
fn review(store: &Store<'_>, editor: Option<&str>, json: bool, style: render::Style) -> Outcome {
    let proposed = store.list(Some(KnowledgeStatus::Proposed))?;
    if proposed.is_empty() {
        return emit(json, &serde_json::json!({"decisions": []}), || {
            "nothing to review: no proposed knowledge\n".into()
        });
    }
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    let mut decisions = Vec::new();
    let total = proposed.len();
    for (n, entry) in proposed.into_iter().enumerate() {
        let mut err = io::stderr();
        let _ = write!(
            err,
            "\n[{}/{total}]\n{}[a]dopt, [r]eject, [e]dit then adopt, [s]kip, [q]uit? ",
            n + 1,
            render_entry(&entry, style)
        );
        let _ = err.flush();
        let Some(answer) = lines.next().transpose()? else {
            break;
        };
        let answer = answer.trim().to_lowercase();
        let decision = match answer.as_str() {
            "a" | "adopt" | "y" | "yes" => {
                let adopted = store.adopt(entry.id)?;
                Some(("adopted", adopted))
            }
            "r" | "reject" | "n" | "no" => {
                let _ = write!(err, "reason (optional): ");
                let _ = err.flush();
                let reason = lines.next().transpose()?.unwrap_or_default();
                let reason = Some(reason.trim()).filter(|r| !r.is_empty());
                Some(("rejected", store.reject(entry.id, reason)?))
            }
            "e" | "edit" => {
                let text = edit_text(&entry.text, editor, &format!("knowledge-{}", entry.id))?;
                store.edit(
                    entry.id,
                    &KnowledgeEditRequest {
                        text: Some(text),
                        path: None,
                        kind: None,
                    },
                )?;
                Some(("edited and adopted", store.adopt(entry.id)?))
            }
            "q" | "quit" => break,
            _ => None,
        };
        if let Some((what, entry)) = decision {
            if !json {
                eprintln!("{what} k{}", entry.id);
            }
            decisions.push(serde_json::json!({"decision": what, "entry": entry}));
        }
    }
    emit(json, &serde_json::json!({"decisions": decisions}), || {
        format!("{} decision(s) made\n", decisions.len())
    })
}
