//! Approvals, the effect ledger and undo through the engine, against a mock
//! gateway on loopback (`mock_gateway`) and the fake ACP agent, whose turns
//! call the gateway the way Anvil's packaged SDKs do. Hermetic: nothing
//! leaves the machine.

mod common;
#[path = "mock_gateway/mod.rs"]
mod mock_gateway;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use branchyard::connectors::{Bundle, Gateway, GrantEntry, Packager};
use branchyard::effects::{
    Approval, ApprovalPolicy, ApprovalSettings, AskAbout, EffectClass, EffectState, Layer, UndoKind,
};
use branchyard::{
    Activity, BranchStatus, Budget, DecisionSource, Envelope, Error, Policy, Provisioning, Spawn,
    TaskOptions,
};
use common::{fake_agent, text, Fixture};
use mock_gateway::MockGateway;

const BUNDLES: [&str; 6] = ["slack", "github", "gmail", "webhook", "legacy", "flaky"];

#[derive(Debug)]
struct FakePackager;

impl Packager for FakePackager {
    fn served(&self) -> Result<Vec<Bundle>, String> {
        Ok(BUNDLES
            .iter()
            .map(|id| Bundle {
                id: (*id).to_owned(),
                path: PathBuf::from(format!("/bundles/{id}")),
                hash: format!("hash-{id}"),
            })
            .collect())
    }

    fn package(&self, bundle: &Bundle, out: &Path) -> Result<(), String> {
        fs::write(out.join("SKILL.md"), format!("# {}\n", bundle.id)).map_err(|e| e.to_string())
    }

    fn index(&self, _grants: &Path, _bundles: &[Bundle], out: &Path) -> Result<(), String> {
        fs::write(out, "# Connectors\n").map_err(|e| e.to_string())
    }
}

/// A gateway for `f`'s yard, approvals as given, and the harness's client
/// script written beside the repository.
fn setup(f: &Fixture, approvals: ApprovalSettings) -> (MockGateway, String) {
    let mock = MockGateway::start();
    f.yard.use_connectors(
        Gateway::local(&f.yard, &mock.url, Arc::new(FakePackager)).expect("a local gateway"),
    );
    f.yard.use_approvals(approvals);
    let script = f.dir.join("call.py");
    fs::write(&script, mock_gateway::CALL_PY).unwrap();
    (mock, script.display().to_string())
}

fn policy(rules: &[(&str, Approval)]) -> ApprovalPolicy {
    ApprovalPolicy {
        rules: rules.iter().map(|(p, a)| ((*p).to_owned(), *a)).collect(),
        ..ApprovalPolicy::default()
    }
}

fn person(rules: &[(&str, Approval)]) -> ApprovalSettings {
    ApprovalSettings {
        person: Some(policy(rules)),
        ..ApprovalSettings::default()
    }
}

/// Write grants on every bundle, a private home, and a two-minute budget.
fn granted(f: &Fixture) -> TaskOptions {
    TaskOptions {
        isolated: true,
        provision: Some(Provisioning {
            connectors: BUNDLES
                .iter()
                .map(|b| GrantEntry::parse(&format!("{b}:write")).unwrap())
                .collect(),
            ..Provisioning::default()
        }),
        policy: Policy::allow_all(),
        budget: Budget {
            max_duration: Some(Duration::from_secs(120)),
            ..Budget::default()
        },
        ..f.options()
    }
}

/// `SH` lines calling `tool` with `arguments` through the gateway.
fn calls(script: &str, calls: &[(&str, &str)]) -> String {
    calls
        .iter()
        .map(|(tool, arguments)| format!("SH python3 {script} {tool} '{arguments}'"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Wait for `ready`, polling the store, at most a minute.
fn until<T>(what: &str, mut ready: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(found) = ready() {
            return found;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn an_effect_is_begun_before_its_call_and_finished_from_the_gateways_metadata() {
    let f = Fixture::new();
    let (mock, script) = setup(&f, ApprovalSettings::default());
    // What the ledger said when the call reached the gateway.
    let yard = f.yard.clone();
    mock.on_call(move |call| {
        let key = call.key.as_deref()?;
        let entry = yard.effect(key).ok()?;
        Some(format!("{}:{}", entry.state, entry.id))
    });
    mock.set_deadline(4_102_444_800_000);
    let branch = f
        .task(&calls(
            &script,
            &[
                ("slack__chat_post", r#"{"channel": "board", "text": "hi"}"#),
                ("github__issues_list", "{}"),
            ],
        ))
        .options(granted(&f))
        .name("post")
        .run()
        .unwrap();
    let events = branch.events().unwrap();
    let said = text(&events);
    assert_eq!(branch.info().status, BranchStatus::NoChanges, "{said}");
    assert!(
        said.contains("call slack__chat_post: posted ts=1"),
        "{said}"
    );
    assert!(said.contains("call github__issues_list: []"), "{said}");
    // One entry: the read is not an effect.
    let ledger = f.yard.effects(Some("post")).unwrap();
    assert_eq!(ledger.len(), 1, "{ledger:?}");
    let entry = &ledger[0];
    assert_eq!(entry.state, EffectState::Confirmed);
    assert_eq!(entry.class, EffectClass::Reversible);
    assert!(entry.declared);
    assert_eq!(entry.connector, "slack");
    assert_eq!(entry.operation, "chat_post");
    assert_eq!(entry.turn, 1);
    assert_eq!(entry.task, "post");
    assert_eq!(entry.summary.as_deref(), Some("message in #board"));
    assert!(entry.request_digest.starts_with("blake3:"));
    assert!(!serde_json::to_string(entry).unwrap().contains("\"hi\""));
    let undo = entry.undo.as_ref().unwrap();
    assert_eq!(undo.operation, "chat_delete");
    assert_eq!(undo.kind, UndoKind::Inverse);
    assert_eq!(undo.deadline_ms, Some(4_102_444_800_000));
    assert_eq!(entry.approval.as_ref().unwrap().surface, "policy");
    // The gateway got the entry's id as the key, in the header and in
    // `_meta`, while the entry was begun.
    let post = &mock.calls_to("slack__chat_post")[0];
    assert_eq!(post.key.as_deref(), Some(entry.id.as_str()));
    assert_eq!(post.meta["idempotency_key"], entry.id);
    assert_eq!(
        post.seen.as_deref(),
        Some(format!("begun:{}", entry.id).as_str())
    );
    // Its history: opened begun, then confirmed.
    let history = f.yard.effect_history(&entry.id).unwrap();
    assert_eq!(history.len(), 2);
    // Recorded on the branch, and the proxy said where it was.
    let activity: Vec<String> = events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Effect(a) => Some(a.describe()),
            _ => None,
        })
        .collect();
    assert!(
        activity[0].starts_with("effects: connector calls are ledgered at http://127.0.0.1:"),
        "{activity:?}"
    );
    assert!(
        activity
            .iter()
            .any(|a| a.contains("slack chat_post confirmed (reversible)")),
        "{activity:?}"
    );
}

#[test]
fn a_call_the_gateway_describes_nothing_about_is_irreversible() {
    let f = Fixture::new();
    let (mock, script) = setup(&f, person(&[("legacy:*", Approval::Allow)]));
    let branch = f
        .task(&calls(&script, &[("legacy__do", "{}")]))
        .options(granted(&f))
        .name("legacy")
        .run()
        .unwrap();
    assert!(text(&branch.events().unwrap()).contains("call legacy__do: done"));
    let entry = &f.yard.effects(Some("legacy")).unwrap()[0];
    assert_eq!(entry.state, EffectState::Confirmed);
    assert_eq!(entry.class, EffectClass::Irreversible);
    assert!(!entry.declared);
    assert_eq!(entry.undo, None);
    assert!(entry
        .detail
        .as_deref()
        .unwrap()
        .contains("described no effect"));
    // Its default would have staged it: the person's rule allowed it.
    assert_eq!(entry.decided.as_ref().unwrap().layer, Layer::Person);
    assert_eq!(mock.calls_to("legacy__do").len(), 1);
}

#[test]
fn an_administrators_lock_cannot_be_loosened() {
    let f = Fixture::new();
    let settings = ApprovalSettings {
        admin: Some(policy(&[("slack:*", Approval::Block)])),
        person: Some(policy(&[("slack:*", Approval::Allow)])),
        ..ApprovalSettings::default()
    };
    let (mock, script) = setup(&f, settings);
    let branch = f
        .task(&calls(
            &script,
            &[("slack__chat_post", r#"{"channel": "x"}"#)],
        ))
        .options(granted(&f))
        .name("locked")
        .run()
        .unwrap();
    let said = text(&branch.events().unwrap());
    assert!(said.contains("call slack__chat_post error:"), "{said}");
    assert!(said.contains("approval_blocked"), "{said}");
    assert!(mock.calls_to("slack__chat_post").is_empty());
    assert!(f.yard.effects(None).unwrap().is_empty());
    let blocked = branch
        .events()
        .unwrap()
        .into_iter()
        .find_map(|e| match e.activity {
            Activity::Effect(a) => match *a {
                branchyard::effects::EffectActivity::Blocked { resolved, .. } => Some(resolved),
                _ => None,
            },
            _ => None,
        })
        .unwrap();
    assert_eq!(blocked.layer, Layer::Admin);
}

#[test]
fn a_deletion_asks_even_when_allowed_and_the_answer_is_recorded() {
    let f = Fixture::new();
    let (mock, script) = setup(&f, person(&[("slack:*", Approval::Allow)]));
    let options = granted(&f);
    std::thread::scope(|scope| {
        let run = scope.spawn(|| {
            f.task(&calls(&script, &[("slack__chat_delete", r#"{"ts": "9"}"#)]))
                .options(options.clone())
                .name("deleter")
                .run()
        });
        let ask = until("an ask", || {
            f.yard.approvals(true).unwrap().into_iter().next()
        });
        assert_eq!(ask.branch, "deleter");
        assert!(
            matches!(&ask.about, AskAbout::Operation { deletion: true, operation, .. } if operation == "chat_delete")
        );
        assert_eq!(ask.resolved.as_ref().unwrap().layer, Layer::Deletion);
        assert_eq!(ask.request.as_ref().unwrap()["ts"], "9");
        assert!(
            ask.deadline_ms.is_some(),
            "it waits within the turn's budget"
        );
        // Nothing reached the gateway yet.
        assert!(mock.calls_to("slack__chat_delete").is_empty());
        let answered = f
            .yard
            .answer_approval(
                &ask.id[ask.id.len() - 8..],
                true,
                "ana",
                "api",
                Some("fine"),
            )
            .unwrap();
        assert_eq!(answered.answer.as_ref().unwrap().by, "ana");
        // Answered once.
        assert!(matches!(
            f.yard.answer_approval(&ask.id, false, "bob", "cli", None),
            Err(Error::Denied(_))
        ));
        let branch = run.join().unwrap().unwrap();
        let said = text(&branch.events().unwrap());
        assert!(said.contains("call slack__chat_delete: deleted"), "{said}");
    });
    let entry = &f.yard.effects(Some("deleter")).unwrap()[0];
    assert!(entry.deletion);
    assert_eq!(entry.state, EffectState::Confirmed);
    let approval = entry.approval.as_ref().unwrap();
    assert_eq!(
        (approval.by.as_str(), approval.surface.as_str()),
        ("ana", "api")
    );
    assert_eq!(mock.calls_to("slack__chat_delete").len(), 1);
}

#[test]
fn a_denied_ask_is_never_called() {
    let f = Fixture::new();
    let (mock, script) = setup(&f, ApprovalSettings::default());
    let options = granted(&f);
    std::thread::scope(|scope| {
        let run = scope.spawn(|| {
            f.task(&calls(
                &script,
                &[("github__issues_create", r#"{"title": "t"}"#)],
            ))
            .options(options.clone())
            .name("asker")
            .run()
        });
        // Compensable asks by default.
        let ask = until("an ask", || {
            f.yard.approvals(true).unwrap().into_iter().next()
        });
        assert_eq!(ask.resolved.as_ref().unwrap().layer, Layer::Default);
        f.yard
            .answer_approval(&ask.id, false, "ana", "watch", Some("not now"))
            .unwrap();
        let said = text(&run.join().unwrap().unwrap().events().unwrap());
        assert!(said.contains("approval_denied"), "{said}");
        assert!(said.contains("denied by ana (watch): not now"), "{said}");
    });
    assert!(mock.calls_to("github__issues_create").is_empty());
    assert!(f.yard.effects(None).unwrap().is_empty());
}

#[test]
fn a_tool_approval_asks_and_never_loosens_the_policy() {
    let f = Fixture::new();
    f.yard.use_approvals(person(&[("write *", Approval::Ask)]));
    std::thread::scope(|scope| {
        let run = scope.spawn(|| {
            f.task("PERMISSION WRITE p.txt=1")
                .policy(Policy::allow_all())
                .name("tool")
                .run()
        });
        let ask = until("an ask", || {
            f.yard.approvals(true).unwrap().into_iter().next()
        });
        assert_eq!(
            ask.about,
            AskAbout::Tool {
                tool: "write marker".into()
            }
        );
        f.yard
            .answer_approval(&ask.id, true, "ana", "cli", None)
            .unwrap();
        let branch = run.join().unwrap().unwrap();
        assert!(branch.info().worktree.join("p.txt").is_file());
        let source = branch
            .events()
            .unwrap()
            .into_iter()
            .find_map(|e| match e.activity {
                Activity::Decision { source, .. } => Some(source),
                _ => None,
            })
            .unwrap();
        assert!(matches!(source, DecisionSource::Approval { by: Some(by), .. } if by == "ana"));
    });
    // An approval that allows does not override a policy that denies.
    f.yard
        .use_approvals(person(&[("write *", Approval::Allow)]));
    let denied = f
        .task("PERMISSION WRITE q.txt=1")
        .policy(Policy::deny_all())
        .name("denied")
        .run()
        .unwrap();
    assert!(!denied.info().worktree.join("q.txt").exists());
}

#[test]
fn a_childs_ask_is_escalated_to_its_parent_which_answers_it() {
    let f = Fixture::new();
    f.yard.use_approvals(person(&[("write *", Approval::Ask)]));
    let options = TaskOptions {
        delegation: Some(Envelope::depth(1)),
        delegation_server: Some(vec![fake_agent().display().to_string()]),
        policy: Policy::allow_all(),
        ..f.options()
    };
    let root = f
        .yard
        .task("WRITE root.txt=r")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let delegate = root.delegate(options.clone()).unwrap();
    delegate
        .spawn(Spawn {
            prompt: "PERMISSION WRITE kid.txt=x".into(),
            name: Some("kid".into()),
            ..Spawn::default()
        })
        .unwrap();
    let ask = until("the child's ask", || {
        f.yard
            .approvals(true)
            .unwrap()
            .into_iter()
            .find(|a| a.branch == "kid")
    });
    // It reached the parent's inbox.
    let inbox = until("the escalation", || {
        delegate
            .inbox()
            .unwrap()
            .messages
            .into_iter()
            .find(|m| m.from == "kid" && m.text.contains(&ask.id))
    });
    assert_eq!(inbox.kind, branchyard::MessageKind::Escalation);
    // Only an ancestor may answer.
    let other = f
        .yard
        .task("WRITE other.txt=o")
        .options(options.clone())
        .name("other")
        .run()
        .unwrap();
    assert!(matches!(
        other
            .delegate(options.clone())
            .unwrap()
            .answer_approval(&ask.id, true, None),
        Err(Error::Denied(_))
    ));
    let answered = delegate.answer_approval(&ask.id, true, None).unwrap();
    let answer = answered.answer.unwrap();
    assert_eq!(
        (answer.by.as_str(), answer.surface.as_str()),
        ("root", "parent")
    );
    root.wait_subtree().unwrap();
    let kid = f.yard.branch("kid").unwrap();
    assert_eq!(kid.info().status, BranchStatus::Ready);
    assert!(kid.info().worktree.join("kid.txt").is_file());
}

#[test]
fn a_lost_answer_is_unknown_until_the_lookup_settles_it() {
    let f = Fixture::new();
    let (mock, script) = setup(&f, person(&[("flaky:*", Approval::Allow)]));
    let branch = f
        .task(&calls(&script, &[("flaky__charge", r#"{"amount": 5}"#)]))
        .options(granted(&f))
        .name("flaky")
        .run()
        .unwrap();
    let said = text(&branch.events().unwrap());
    assert!(
        said.contains("call flaky__charge failed: HTTP Error 502"),
        "{said}"
    );
    let entry = f.yard.effects(Some("flaky")).unwrap().remove(0);
    assert_eq!(entry.state, EffectState::Unknown);
    assert!(
        entry.detail.as_deref().unwrap().contains("lost"),
        "{entry:?}"
    );
    // Never retried: one call reached the gateway.
    assert_eq!(mock.calls_to("flaky__charge").len(), 1);
    let report = f.yard.reconcile_effects().unwrap();
    assert_eq!(report.settled, [(entry.id.clone(), EffectState::Confirmed)]);
    let lookup = &mock.calls_to("flaky__charge_lookup")[0];
    assert_eq!(lookup.arguments["idempotency_key"], entry.id);
    assert_eq!(lookup.arguments["kind"], "charge");
    let settled = f.yard.effect(&entry.id).unwrap();
    assert_eq!(settled.state, EffectState::Confirmed);
    assert_eq!(settled.undo.as_ref().unwrap().operation, "refund");
    assert_eq!(mock.calls_to("flaky__charge").len(), 1);
}

#[test]
fn an_irreversible_call_is_staged_as_a_draft_or_held_then_promoted() {
    let f = Fixture::new();
    let (mock, script) = setup(&f, ApprovalSettings::default());
    let branch = f
        .task(&calls(
            &script,
            &[
                ("gmail__send", r#"{"to": "finance@"}"#),
                ("webhook__fire", r#"{"url": "https://x"}"#),
            ],
        ))
        .options(granted(&f))
        .name("stager")
        .run()
        .unwrap();
    let said = text(&branch.events().unwrap());
    assert!(said.contains("call gmail__send: draft d-1"), "{said}");
    assert!(
        said.contains("call webhook__fire: Branchyard staged this call"),
        "{said}"
    );
    let ledger = f.yard.effects(Some("stager")).unwrap();
    let (mail, hook) = (&ledger[0], &ledger[1]);
    assert_eq!(mail.state, EffectState::Staged);
    assert_eq!(mail.staged.as_ref().unwrap().draft.as_deref(), Some("d-1"));
    assert_eq!(hook.state, EffectState::Staged);
    assert_eq!(hook.staged.as_ref().unwrap().draft, None);
    // The draft was made with stage: true; the outbox call was not made.
    let draft = &mock.calls_to("gmail__send")[0];
    assert_eq!(draft.meta["stage"], true);
    assert!(mock.calls_to("webhook__fire").is_empty());
    // Two approvals wait to promote them.
    let waiting = f.yard.approvals(true).unwrap();
    assert_eq!(waiting.len(), 2);
    assert!(waiting
        .iter()
        .all(|a| matches!(a.about, AskAbout::Promote { .. })));

    // Promoting the draft performs it, under the same key.
    let sent = f.yard.promote_effect(&mail.id, "ana", "cli").unwrap();
    assert_eq!(sent.state, EffectState::Confirmed);
    assert_eq!(sent.approval.as_ref().unwrap().by, "ana");
    let promote = &mock.calls_to("gmail__send")[1];
    assert_eq!(promote.meta["promote"], "d-1");
    assert_eq!(promote.key.as_deref(), Some(mail.id.as_str()));
    // Approving the outbox's ask makes the held call.
    let held = waiting
        .iter()
        .find(|a| a.effect.as_deref() == Some(hook.id.as_str()))
        .unwrap();
    f.yard
        .answer_approval(&held.id, true, "ana", "companion", None)
        .unwrap();
    let fired = f.yard.effect(&hook.id).unwrap();
    assert_eq!(fired.state, EffectState::Confirmed);
    assert_eq!(
        mock.calls_to("webhook__fire")[0].arguments["url"],
        "https://x"
    );
    assert!(f.yard.approvals(true).unwrap().is_empty());
    // A promoted effect cannot be promoted again.
    assert!(f.yard.promote_effect(&hook.id, "ana", "cli").is_err());
}

#[test]
fn undo_plans_by_what_the_upstream_supports_and_undoes_what_was_chosen() {
    let f = Fixture::new();
    let (mock, script) = setup(
        &f,
        person(&[("github:*", Approval::Allow), ("legacy:*", Approval::Allow)]),
    );
    mock.set_deadline(4_102_444_800_000);
    let first = f
        .task(&calls(
            &script,
            &[("slack__chat_post", r#"{"channel": "board"}"#)],
        ))
        .options(granted(&f))
        .name("board")
        .run()
        .unwrap();
    let branch = first
        .send(
            &calls(
                &script,
                &[
                    ("slack__chat_post", r#"{"channel": "board"}"#),
                    ("github__issues_create", r#"{"title": "prep"}"#),
                    ("legacy__do", "{}"),
                    ("webhook__fire", "{}"),
                ],
            ),
            TaskOptions {
                policy: Policy::allow_all(),
                ..TaskOptions::default()
            },
        )
        .unwrap();
    let ledger = f.yard.effects(Some("board")).unwrap();
    assert_eq!(ledger.len(), 5, "{ledger:?}");
    // Back to turn 1: the first post stays out of the plan.
    let plan = f.yard.undo_plan_at("board", 1, 1_000_000).unwrap();
    assert_eq!(plan.reversible.len(), 1);
    assert_eq!(plan.compensable.len(), 1);
    assert_eq!(plan.irreversible.len(), 1);
    assert_eq!(plan.staged.len(), 1);
    assert!(plan.unknown.is_empty());
    let rendered = branchyard::effects::undo::render(&plan, "board update", true);
    assert!(
        rendered.starts_with("Rewinding \"board update\" to turn 1:\n"),
        "{rendered}"
    );
    assert!(
        rendered.contains("  files and conversation        restored exactly\n"),
        "{rendered}"
    );
    assert!(
        rendered.contains("  upstream, can be undone       slack: message in #board"),
        "{rendered}"
    );
    assert!(
        rendered.contains("  upstream, can be compensated  github: issue #"),
        "{rendered}"
    );
    assert!(
        rendered.contains("will be closed, not deleted"),
        "{rendered}"
    );
    assert!(
        rendered.contains("  upstream, cannot be undone    legacy: do"),
        "{rendered}"
    );
    assert!(
        rendered.contains("the gateway described no undo"),
        "{rendered}"
    );
    // Partial undo: the reversible post only.
    let post = plan.reversible[0].entry.clone();
    let outcomes = f
        .yard
        .undo_effects_at(
            &plan,
            &[post.id[post.id.len() - 6..].to_owned()],
            "ana",
            "cli",
            1_000_000,
        )
        .unwrap();
    assert_eq!(outcomes[0].state, EffectState::Undone, "{outcomes:?}");
    let delete = &mock.calls_to("slack__chat_delete")[0];
    assert_eq!(delete.arguments, post.undo.as_ref().unwrap().arguments);
    assert_eq!(
        delete.key.as_deref(),
        Some(format!("{}-undo", post.id).as_str())
    );
    let undone = f.yard.effect(&post.id).unwrap();
    assert_eq!(undone.state, EffectState::Undone);
    assert_eq!(undone.undo_approval.as_ref().unwrap().by, "ana");
    assert_eq!(
        undone.approval.as_ref().unwrap().surface,
        "policy",
        "the original's kept"
    );
    // The compensable one was not touched; compensating it closes the issue.
    let issue = plan.compensable[0].entry.clone();
    assert_eq!(
        f.yard.effect(&issue.id).unwrap().state,
        EffectState::Confirmed
    );
    let outcomes = f
        .yard
        .undo_effects_at(&plan, &[issue.id.clone()], "ana", "cli", 1_000_000)
        .unwrap();
    assert_eq!(outcomes[0].state, EffectState::Compensated);
    assert!(mock.calls_to("github__issues_close")[0].arguments["number"].is_u64());
    // Undoing the staged call discards it; it never happened.
    let staged = plan.staged[0].entry.clone();
    let outcomes = f
        .yard
        .undo_effects_at(&plan, &[staged.id.clone()], "ana", "cli", 1_000_000)
        .unwrap();
    assert_eq!(outcomes[0].state, EffectState::Failed);
    assert!(mock.calls_to("webhook__fire").is_empty());
    // The whole task: the first turn's post is planned too.
    let all = f.yard.undo_plan_at("board", 0, 1_000_000).unwrap();
    assert_eq!(all.reversible.len(), 1);
    assert_eq!(all.reversible[0].entry.turn, 1);
    let _ = branch;
}

#[test]
fn an_expired_undo_is_not_called_and_a_failed_one_says_why() {
    let f = Fixture::new();
    let (mock, script) = setup(&f, ApprovalSettings::default());
    mock.set_deadline(5_000_000);
    f.task(&calls(
        &script,
        &[
            ("slack__chat_post", r#"{"channel": "a"}"#),
            ("slack__chat_post", r#"{"channel": "b"}"#),
        ],
    ))
    .options(granted(&f))
    .name("late")
    .run()
    .unwrap();
    // Before the deadline, both are undoable; after it, neither is.
    let before = f.yard.undo_plan_at("late", 0, 4_000_000).unwrap();
    assert_eq!(before.reversible.len(), 2);
    let after = f.yard.undo_plan_at("late", 0, 6_000_000).unwrap();
    assert_eq!(after.irreversible.len(), 2);
    assert!(after.irreversible[0].note.contains("expired"));
    let (a, b) = (
        before.reversible[0].entry.id.clone(),
        before.reversible[1].entry.id.clone(),
    );
    // Chosen from an older plan, past the deadline: expired, not called.
    let outcomes = f
        .yard
        .undo_effects_at(&before, &[a.clone()], "ana", "cli", 6_000_000)
        .unwrap();
    assert_eq!(outcomes[0].state, EffectState::Expired);
    assert!(mock.calls_to("slack__chat_delete").is_empty());
    // Reconciliation marks what expired too.
    let report = f.yard.reconcile_effects_at(6_000_000).unwrap();
    assert_eq!(report.settled, [(b.clone(), EffectState::Expired)]);
    // An inverse the upstream refuses: undo_failed, with its answer.
    let f2 = Fixture::new();
    let (mock, script) = setup(&f2, ApprovalSettings::default());
    mock.fail_inverses();
    f2.task(&calls(
        &script,
        &[("slack__chat_post", r#"{"channel": "a"}"#)],
    ))
    .options(granted(&f2))
    .name("refused")
    .run()
    .unwrap();
    let plan = f2.yard.undo_plan("refused", 0).unwrap();
    let id = plan.reversible[0].entry.id.clone();
    let outcomes = f2
        .yard
        .undo_effects(&plan, &[id.clone()], "ana", "cli")
        .unwrap();
    assert_eq!(outcomes[0].state, EffectState::UndoFailed);
    let failed = f2.yard.effect(&id).unwrap();
    assert_eq!(failed.state, EffectState::UndoFailed);
    assert!(
        failed
            .detail
            .as_deref()
            .unwrap()
            .contains("it is already gone"),
        "{failed:?}"
    );
    // It stays in the plan, to try again.
    assert_eq!(f2.yard.undo_plan("refused", 0).unwrap().reversible.len(), 1);
}
