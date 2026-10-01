//! Repository knowledge, plan approval and goals over real HTTP, against
//! the fake ACP agent: the `/knowledge` routes as a person's decisions,
//! adopted entries given to a task's harness, a planned task approved as an
//! operation, and a goal's judge falling back when its answer is not a
//! verdict. See `docs/knowledge.md` and `docs/plans-and-goals.md`.

mod common;

use branchyard::{Activity, BranchStatus, GoalActivity, JudgedBy, KnowledgeScope, KnowledgeStatus};
use branchyard_client::api::{
    GoalRequest, OperationKind, OperationState, PolicySpec, SendRequest, TaskRequest,
};
use branchyard_client::knowledge_api::{
    KnowledgeAddRequest, KnowledgeEditRequest, PlanApproveRequest, PlanRejectRequest,
};
use branchyard_client::new_key;
use common::{run, task, wait, Fixture, Server};

#[test]
fn knowledge_is_a_persons_decision_over_http_and_reaches_harnesses() {
    let f = Fixture::new();
    let server = Server::start(f.config());
    let client = server.client();
    let repo = client.repo("app");

    let adopted = repo
        .add_knowledge(&KnowledgeAddRequest {
            text: "Keep commits small.".into(),
            scope: KnowledgeScope::default(),
            propose: false,
        })
        .unwrap();
    assert_eq!(adopted.status, KnowledgeStatus::Adopted);
    assert_eq!(adopted.adopted_by.as_deref(), Some("tester"));
    let proposed = repo
        .add_knowledge(&KnowledgeAddRequest {
            text: "Docs use sentence case.".into(),
            scope: KnowledgeScope {
                path: None,
                kind: Some(branchyard::TaskKind::Docs),
            },
            propose: true,
        })
        .unwrap();
    assert_eq!(proposed.status, KnowledgeStatus::Proposed);
    assert_eq!(repo.knowledge(None).unwrap().len(), 2);
    let waiting = repo.knowledge(Some(KnowledgeStatus::Proposed)).unwrap();
    assert_eq!(waiting, [proposed.clone()]);
    assert_eq!(repo.knowledge_entry(proposed.id).unwrap(), proposed);

    // Edited, then adopted; the export has both.
    let edited = repo
        .edit_knowledge(
            proposed.id,
            &KnowledgeEditRequest {
                text: Some("Documentation uses sentence case.".into()),
                path: None,
                kind: Some(String::new()),
            },
        )
        .unwrap();
    assert_eq!(edited.scope, KnowledgeScope::default());
    repo.adopt_knowledge(proposed.id).unwrap();
    let export = repo.export_knowledge().unwrap();
    assert_eq!(export.entries, [adopted.id, proposed.id]);
    assert!(export
        .markdown
        .contains("Documentation uses sentence case."));

    // A task's harness is given the adopted entries, traced by id.
    let op = run(&client, &task("SHOW_INSTRUCTIONS", "told"));
    assert_eq!(op.state, OperationState::Succeeded, "{op:?}");
    let events = repo.events("told", 0).unwrap().events;
    let given: Vec<Vec<u64>> = events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Provisioned { knowledge, .. } => Some(knowledge.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(given, [vec![adopted.id, proposed.id]]);

    // A correction sent through the server becomes a proposal on demand.
    let op = run(&client, &task("WRITE notes.txt=1", "notes"));
    assert_eq!(op.state, OperationState::Succeeded);
    let sent = repo
        .send(
            "notes",
            &SendRequest {
                prompt: "Always date each note WRITE notes.txt=2".into(),
                policy: PolicySpec::allow_all(),
                ..SendRequest::default()
            },
            &new_key(),
        )
        .unwrap();
    assert_eq!(wait(&client, &sent.id).state, OperationState::Succeeded);
    let distilled = repo.distill("notes").unwrap();
    assert_eq!(distilled.proposed.len(), 1);
    assert_eq!(
        distilled.proposed[0].text,
        "Always date each note WRITE notes.txt=2"
    );
    let rejected = repo
        .reject_knowledge(distilled.proposed[0].id, Some("too narrow"))
        .unwrap();
    assert_eq!(rejected.status, KnowledgeStatus::Rejected);
    assert_eq!(rejected.note.as_deref(), Some("too narrow"));

    // Removed, and then unknown.
    repo.remove_knowledge(adopted.id).unwrap();
    let missing = repo.knowledge_entry(adopted.id).unwrap_err();
    assert_eq!(missing.code(), Some("unknown_knowledge"));
}

#[test]
fn a_planned_task_waits_and_its_approval_is_an_operation() {
    let f = Fixture::new();
    let server = Server::start(f.config());
    let client = server.client();
    let repo = client.repo("app");

    let op = run(
        &client,
        &TaskRequest {
            plan: true,
            ..task("Mark it PERMISSION WRITE marker.txt=x", "planned")
        },
    );
    assert_eq!(op.state, OperationState::Succeeded, "{op:?}");
    let info = repo.branch("planned").unwrap();
    assert_eq!(info.status, BranchStatus::AwaitingPlanApproval);
    let plan = repo.plan("planned").unwrap();
    assert_eq!(plan.phase, branchyard::PlanPhase::Awaiting);
    assert_eq!(plan.plan.unwrap().markdown, "denied");

    let approved = repo
        .approve_plan(
            "planned",
            &PlanApproveRequest {
                edited: Some("WRITE approved.txt=1".into()),
                send: SendRequest {
                    policy: PolicySpec::allow_all(),
                    ..SendRequest::default()
                },
            },
            &new_key(),
        )
        .unwrap();
    assert_eq!(approved.kind, OperationKind::ApprovePlan);
    let done = wait(&client, &approved.id);
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    let info = repo.branch("planned").unwrap();
    assert_eq!(info.status, BranchStatus::Ready);
    assert!(info.worktree.join("approved.txt").is_file());

    // Nothing awaits now: refused at once, as a conflict.
    let refused = repo
        .reject_plan(
            "planned",
            &PlanRejectRequest {
                reason: Some("late".into()),
                ..PlanRejectRequest::default()
            },
            &new_key(),
        )
        .unwrap_err();
    assert_eq!(refused.code(), Some("no_plan"));
}

#[test]
fn a_goal_judge_that_answers_no_verdict_falls_back_over_http() {
    let f = Fixture::new();
    let server = Server::start(f.config());
    let client = server.client();
    let repo = client.repo("app");
    let op = run(
        &client,
        &TaskRequest {
            goal: Some(GoalRequest {
                text: "a file is written".into(),
                rounds: Some(0),
                // The server's gemini-cli is the fake agent, which echoes:
                // not a verdict.
                judge: Some("gemini-cli".into()),
            }),
            ..task("WRITE written.txt=1", "goal")
        },
    );
    assert_eq!(op.state, OperationState::Succeeded, "{op:?}");
    assert_eq!(repo.branch("goal").unwrap().status, BranchStatus::Ready);
    let events = repo.events("goal", 0).unwrap().events;
    let by = events
        .iter()
        .find_map(|e| match &e.activity {
            Activity::Goal(g) => match g.as_ref() {
                GoalActivity::Verdict { by, met: true, .. } => Some(by.clone()),
                _ => None,
            },
            _ => None,
        })
        .expect("a verdict");
    assert!(matches!(by, JudgedBy::Fallback { .. }), "{by:?}");
}
