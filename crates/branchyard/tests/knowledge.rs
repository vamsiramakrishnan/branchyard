//! Repository knowledge against the fake ACP agent: entries a person adopts
//! are given to matching branches, most specific first, within a budget,
//! and traced in the `provisioned` event; a branch's end proposes entries
//! from the corrections it was sent, never adopting them; a harness
//! distiller answers a canned file. No model is called.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::sync::Arc;

use branchyard::{
    Activity, DistillTrigger, Error, JudgeSpec, JudgedBy, KnowledgeActivity, KnowledgeEdit,
    KnowledgeScope, KnowledgeSettings, KnowledgeSource, KnowledgeStatus, NewKnowledge,
    RecordedEvent, TaskKind, TaskOptions,
};
use common::{fake_agent, text, Fixture};

fn add(f: &Fixture, text: &str, path: Option<&str>, kind: Option<TaskKind>) -> u64 {
    f.yard
        .add_knowledge(
            &NewKnowledge {
                text: text.into(),
                scope: KnowledgeScope {
                    path: path.map(str::to_owned),
                    kind,
                },
                propose: false,
                note: None,
            },
            "ana",
        )
        .unwrap()
        .id
}

fn provisioned(events: &[RecordedEvent]) -> Vec<Vec<u64>> {
    events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Provisioned { knowledge, .. } => Some(knowledge.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn adopted_entries_are_given_most_specific_first_and_traced() {
    let f = Fixture::new();
    let repo = add(&f, "Keep commits small.", None, None);
    let bugfix = add(&f, "Add a regression test.", None, Some(TaskKind::Bugfix));
    let text_files = add(&f, "Text files end with a newline.", Some("*.txt"), None);
    let elsewhere = add(&f, "Never shown: src only.", Some("src/**"), None);
    let docs = add(&f, "Never shown: docs only.", None, Some(TaskKind::Docs));
    let proposed = f
        .yard
        .add_knowledge(
            &NewKnowledge {
                text: "Never shown: proposed.".into(),
                propose: true,
                ..NewKnowledge::default()
            },
            "ana",
        )
        .unwrap();
    assert_eq!(proposed.status, KnowledgeStatus::Proposed);
    let rejected = add(&f, "Never shown: rejected.", None, None);
    f.yard
        .reject_knowledge(rejected, "ana", Some("wrong"))
        .unwrap();

    // "Fix" makes it a bugfix; the prompt names a.txt, a file of the repo.
    let branch = f.task("Fix a.txt SHOW_INSTRUCTIONS").run().unwrap();
    let events = branch.events().unwrap();
    assert_eq!(provisioned(&events), [vec![text_files, bugfix, repo]]);
    let reply = text(&events);
    let at = |needle: &str| {
        reply
            .find(needle)
            .unwrap_or_else(|| panic!("{needle} in {reply}"))
    };
    assert!(
        at(&format!("[k{text_files}] (path *.txt)")) < at(&format!("[k{bugfix}] (kind bugfix)"))
    );
    assert!(at(&format!("[k{bugfix}]")) < at(&format!("[k{repo}] Keep commits small.")));
    assert!(!reply.contains("Never shown"));
    let _ = (elsewhere, docs);

    // A send gets them again, now with the files its candidate changed.
    let sent = branch.send("SHOW_INSTRUCTIONS", f.options()).unwrap();
    assert_eq!(provisioned(&sent.events().unwrap()).len(), 2);

    // A small budget keeps the most specific that fit and says what not.
    f.yard.use_knowledge(KnowledgeSettings {
        budget_tokens: 50,
        ..KnowledgeSettings::default()
    });
    let small = f.task("Fix a.txt SHOW_INSTRUCTIONS").run().unwrap();
    let events = small.events().unwrap();
    assert_eq!(provisioned(&events), [vec![text_files]]);
    assert!(events.iter().any(|e| matches!(
        &e.activity,
        Activity::Warning(w) if w.contains(&format!("#{bugfix}, #{repo} matched but did not fit the 50-token budget"))
    )));

    // Off: nothing is given.
    f.yard.use_knowledge(KnowledgeSettings {
        provision: false,
        ..KnowledgeSettings::default()
    });
    let off = f.task("Fix a.txt SHOW_INSTRUCTIONS").run().unwrap();
    assert!(provisioned(&off.events().unwrap()).is_empty());
}

#[test]
fn a_merged_branch_proposes_its_corrections_and_nothing_is_used_until_adopted() {
    let f = Fixture::new();
    let branch = f.task("Add notes WRITE notes.txt=1").run().unwrap();
    let name = branch.info().name.clone();
    branch
        .send(
            "Always end notes with a summary line WRITE notes.txt=2",
            f.options(),
        )
        .unwrap();
    branch
        .send(
            "File: notes.txt\nLine: 1\nUser comment: \"Use sentence case in notes\"",
            f.options(),
        )
        .unwrap();
    branch.send("continue", f.options()).unwrap();
    f.yard.merge(&name, "main").unwrap();

    // Merged: distilled on its own, into proposals.
    let proposed = f.yard.knowledge(Some(KnowledgeStatus::Proposed)).unwrap();
    let texts: Vec<(&str, Option<&str>)> = proposed
        .iter()
        .map(|e| (e.text.as_str(), e.scope.path.as_deref()))
        .collect();
    assert_eq!(
        texts,
        [
            (
                "Always end notes with a summary line WRITE notes.txt=2",
                None
            ),
            ("Use sentence case in notes", Some("notes.txt")),
        ]
    );
    assert!(matches!(
        &proposed[1].source,
        KnowledgeSource::Branch { branch, turn: Some(3), via } if *branch == name && via == "review"
    ));
    let distilled = f.yard.branch(&name).unwrap().events().unwrap();
    assert!(distilled.iter().any(|e| matches!(
        &e.activity,
        Activity::Knowledge(k) if matches!(k.as_ref(), KnowledgeActivity::Distilled { trigger, ids, .. } if trigger == "merged" && ids.len() == 2)
    )));

    // Proposed entries are not given to anyone.
    let before = f.task("Note SHOW_INSTRUCTIONS").run().unwrap();
    assert!(provisioned(&before.events().unwrap()).is_empty());

    // Adopted, it is; rejected, it is not, and is never proposed again.
    let summary = proposed[0].id;
    let case = proposed[1].id;
    let adopted = f.yard.adopt_knowledge(summary, "ana").unwrap();
    assert_eq!(adopted.adopted_by.as_deref(), Some("ana"));
    f.yard
        .reject_knowledge(case, "ana", Some("too narrow"))
        .unwrap();
    let after = f.task("Note SHOW_INSTRUCTIONS").run().unwrap();
    let events = after.events().unwrap();
    assert_eq!(provisioned(&events), [vec![summary]]);
    assert!(text(&events).contains("Always end notes with a summary line"));
    let again = f.yard.distill(&name, None).unwrap();
    assert!(again.proposed.is_empty());
    assert_eq!(again.duplicates, 2);
    assert_eq!(again.by, JudgedBy::Deterministic);

    // An edit keeps it adopted; a removal forgets it.
    let edited = f
        .yard
        .edit_knowledge(
            summary,
            &KnowledgeEdit {
                text: Some("End notes with a one-line summary.".into()),
                path: Some(Some("*.txt".into())),
                kind: None,
            },
            "bo",
        )
        .unwrap();
    assert_eq!(edited.status, KnowledgeStatus::Adopted);
    assert_eq!(edited.adopted_by.as_deref(), Some("bo"));
    assert_eq!(edited.scope.path.as_deref(), Some("*.txt"));
    let export = branchyard::export_knowledge(&f.yard.knowledge(None).unwrap());
    assert!(export.contains(&format!("End notes with a one-line summary. (k{summary})")));
    assert!(!export.contains("sentence case"));
    f.yard.remove_knowledge(summary).unwrap();
    assert!(matches!(
        f.yard.knowledge_entry(summary),
        Err(Error::UnknownKnowledge(_))
    ));
}

#[test]
fn entries_are_checked() {
    let f = Fixture::new();
    for (text, path) in [("", None), ("x", Some("[")), (&"y".repeat(601)[..], None)] {
        let refused = f.yard.add_knowledge(
            &NewKnowledge {
                text: text.into(),
                scope: KnowledgeScope {
                    path: path.map(str::to_owned),
                    kind: None,
                },
                ..NewKnowledge::default()
            },
            "ana",
        );
        assert!(matches!(refused, Err(Error::Unsupported(_))), "{refused:?}");
    }
    assert!(matches!(
        f.yard.adopt_knowledge(99, "ana"),
        Err(Error::UnknownKnowledge(99))
    ));
}

#[test]
fn distilling_on_ready_is_configurable() {
    let f = Fixture::new();
    f.yard.use_knowledge(KnowledgeSettings {
        distill_on: vec![DistillTrigger::Ready],
        ..KnowledgeSettings::default()
    });
    let branch = f.task("Add WRITE a.txt=1").run().unwrap();
    branch
        .send("Prefer short file names WRITE a.txt=2", f.options())
        .unwrap();
    let proposed = f.yard.knowledge(Some(KnowledgeStatus::Proposed)).unwrap();
    assert_eq!(proposed.len(), 1);
    assert_eq!(proposed[0].text, "Prefer short file names WRITE a.txt=2");
}

#[test]
fn a_harness_distiller_proposes_and_an_invalid_answer_falls_back() {
    let f = Fixture::new();
    let answer = f.dir.join("distilled.json");
    std::fs::write(
        &answer,
        r#"{"entries": [{"text": "Run the formatter before you finish.", "path": null, "kind": "bugfix", "why": "the person asked twice"}]}"#,
    )
    .unwrap();
    let bad = f.dir.join("bad.txt");
    std::fs::write(&bad, "Here are some ideas...").unwrap();
    let distiller = Arc::new(branchyard::HarnessJudge {
        spec: JudgeSpec {
            harness: "gemini-cli".into(),
            model: None,
            effort: None,
            command: Some(vec![fake_agent().display().to_string()]),
            rubric: None,
        },
        options: TaskOptions::default(),
    });
    // The distiller's prompt quotes the task, so its first line names the
    // canned answer; the branch's own first turn just answers it too.
    let ran = |file: &std::path::Path, name: &str| {
        let branch = f
            .task(&format!("Tidy up\nREPLY_FILE {}", file.display()))
            .name(name)
            .run()
            .unwrap();
        branch
            .send("Run the formatter, please WRITE a.txt=9", f.options())
            .unwrap();
        branch
    };
    let good = ran(&answer, "good");
    let distilled = f.yard.distill("good", Some(distiller.clone())).unwrap();
    assert!(matches!(&distilled.by, JudgedBy::Judge { name } if name == "harness gemini-cli"));
    assert_eq!(distilled.proposed.len(), 1);
    let entry = &distilled.proposed[0];
    assert_eq!(entry.status, KnowledgeStatus::Proposed);
    assert_eq!(entry.scope.kind, Some(TaskKind::Bugfix));
    assert_eq!(entry.note.as_deref(), Some("the person asked twice"));
    assert!(
        matches!(&entry.source, KnowledgeSource::Branch { via, turn: None, .. } if via == "harness gemini-cli")
    );
    let _ = good;

    ran(&bad, "bad");
    let fallback = f.yard.distill("bad", Some(distiller)).unwrap();
    match &fallback.by {
        JudgedBy::Fallback { error, .. } => assert!(error.contains("not a JSON object")),
        other => panic!("{other:?}"),
    }
    // The extractor's proposal stands in.
    assert_eq!(fallback.proposed.len(), 1);
    assert_eq!(
        fallback.proposed[0].text,
        "Run the formatter, please WRITE a.txt=9"
    );
    // The distiller's scratch branches are gone, and were never distilled.
    let names: Vec<String> = f
        .yard
        .branches()
        .unwrap()
        .into_iter()
        .map(|b| b.name)
        .collect();
    assert_eq!(names, ["good", "bad"]);
}
