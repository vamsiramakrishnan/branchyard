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
use std::time::Duration;

use branchyard::connectors::{Bundle, Gateway, GrantEntry, Packager};
use branchyard::effects::{
    Approval, ApprovalPolicy, ApprovalSettings, AskAbout, EffectClass, EffectState, Layer, UndoKind,
};
use branchyard::{
    Activity, BranchStatus, Budget, DecisionSource, Envelope, Error, Policy, Provisioning, Spawn,
    TaskOptions,
};
use branchyard_testkit::wait;
use common::{fake_agent, text, Fixture};
use mock_gateway::MockGateway;

const BUNDLES: [&str; 7] = [
    "slack", "github", "gmail", "webhook", "legacy", "flaky", "blog",
];

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
    assert_eq!(entry.operation_id.as_deref(), Some("slack.chat.post"));
    assert_eq!(entry.title(), "slack: chat_post");
    assert!(entry.request_digest.starts_with("blake3:"));
    assert!(!serde_json::to_string(entry).unwrap().contains("\"hi\""));
    // The undo is the call the gateway named, by its tool.
    let undo = entry.undo.as_ref().unwrap();
    assert_eq!(undo.operation, "slack.chat.delete");
    assert_eq!(undo.tool, "slack__chat_delete");
    assert_eq!(undo.arguments, serde_json::json!({"ts": "1"}));
    assert_eq!(undo.kind, UndoKind::Inverse);
    assert_eq!(undo.deadline_ms, Some(4_102_444_800_000));
    let compensate = entry.compensate.as_ref().unwrap();
    assert_eq!(compensate.tool, "slack__chat_update");
    assert_eq!(compensate.kind, UndoKind::Compensate);
    assert_eq!(entry.upstream_key.as_deref(), Some(entry.id.as_str()));
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
fn a_tasks_record_carries_its_attempts_ledger() {
    let f = Fixture::new();
    let (_mock, script) = setup(&f, ApprovalSettings::default());
    let prompt = format!(
        "SH echo posted > notes.txt\n{}",
        calls(
            &script,
            &[("slack__chat_post", r#"{"channel": "board", "text": "hi"}"#)],
        )
    );
    let branch = f
        .task(&prompt)
        .options(granted(&f))
        .name("noted")
        .run()
        .unwrap();
    assert_eq!(
        branch.info().status,
        BranchStatus::Ready,
        "{}",
        text(&branch.events().unwrap())
    );
    let entry = f.yard.effects(Some("noted")).unwrap().remove(0);
    // The record of checkpoint 1 holds the ledger as it was then, one entry
    // a line, so rewinding the task's history shows what it did upstream.
    let view = branchyard::tasks::list(&f.yard).unwrap().remove(0);
    let record = view.attempts[0].record.clone().unwrap();
    let lines = f.git(&["show", &format!("{record}:.task/effects.jsonl")]);
    let recorded: Vec<serde_json::Value> = lines
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(recorded.len(), 1, "{lines}");
    assert_eq!(recorded[0]["id"], entry.id);
    assert_eq!(recorded[0]["state"], "confirmed");
    assert_eq!(recorded[0]["undo"]["tool"], "slack__chat_delete");
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
        let ask = wait::until("an ask", || {
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
        let ask = wait::until("an ask", || {
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
        let ask = wait::until("an ask", || {
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

/// What a harness might send to slip a call past the ledger: a batch, an
/// encoded body, another token. Prints each status.
const AROUND: &str = r#"import gzip, json, os, urllib.error, urllib.request
url = os.environ["ANVIL_GATEWAY_URL"]
token = open(os.environ["ANVIL_GATEWAY_TOKEN_FILE"]).read().strip()
call = {"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "slack__chat_post", "arguments": {"channel": "x"}}}
def send(name, body, headers):
    h = {"Authorization": "Bearer " + token, "Content-Type": "application/json"}
    h.update(headers)
    try:
        status = urllib.request.urlopen(urllib.request.Request(url, method="POST", data=body, headers=h)).status
    except urllib.error.HTTPError as e:
        status = e.code
    print("around %s: %d" % (name, status))
send("batch", json.dumps([call]).encode(), {})
send("gzip", gzip.compress(json.dumps(call).encode()), {"Content-Encoding": "gzip"})
send("token", json.dumps(call).encode(), {"Authorization": "Bearer someone-else"})
"#;

#[test]
fn a_call_cannot_go_around_the_ledger_through_its_proxy() {
    let f = Fixture::new();
    let (mock, _) = setup(&f, ApprovalSettings::default());
    let around = f.dir.join("around.py");
    fs::write(&around, AROUND).unwrap();
    let branch = f
        .task(&format!("SH python3 {}", around.display()))
        .options(granted(&f))
        .name("around")
        .run()
        .unwrap();
    let said = text(&branch.events().unwrap());
    assert!(said.contains("around batch: 400"), "{said}");
    assert!(said.contains("around gzip: 415"), "{said}");
    assert!(said.contains("around token: 401"), "{said}");
    assert!(mock.calls_to("slack__chat_post").is_empty());
    assert!(f.yard.effects(None).unwrap().is_empty());
}

#[test]
fn an_ask_nobody_answers_expires_with_the_turns_budget() {
    let f = Fixture::new();
    f.yard.use_approvals(person(&[("write *", Approval::Ask)]));
    let branch = f
        .task("PERMISSION WRITE p.txt=1")
        .policy(Policy::allow_all())
        .budget(Budget {
            max_duration: Some(Duration::from_secs(2)),
            ..Budget::default()
        })
        .name("unanswered")
        .run()
        .unwrap();
    assert!(!branch.info().worktree.join("p.txt").exists());
    let ask = f.yard.approvals(false).unwrap().remove(0);
    let answer = ask.answer.unwrap();
    assert!(!answer.allow);
    assert_eq!(
        (answer.by.as_str(), answer.surface.as_str()),
        ("branchyard", "expired")
    );
    assert!(f.yard.approvals(true).unwrap().is_empty());
    // An expired ask cannot be answered later.
    assert!(matches!(
        f.yard.answer_approval(&ask.id, true, "ana", "cli", None),
        Err(Error::Denied(_))
    ));
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
    let ask = wait::until("the child's ask", || {
        f.yard
            .approvals(true)
            .unwrap()
            .into_iter()
            .find(|a| a.branch == "kid")
    });
    // It reached the parent's inbox.
    let inbox = wait::until("the escalation", || {
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
    // No report came back: the lookup is the contract's, resolved from the
    // request and the key when the call was made.
    let lookup = entry.lookup.as_ref().unwrap();
    assert_eq!(lookup.tool, "flaky__charge_lookup");
    assert_eq!(lookup.operation, "flaky.charges.lookup");
    assert_eq!(
        lookup.arguments,
        serde_json::json!({"key": entry.id, "kind": "charge"})
    );
    assert!(
        entry.detail.as_deref().unwrap().contains("lost"),
        "{entry:?}"
    );
    // Never retried: one call reached the gateway.
    assert_eq!(mock.calls_to("flaky__charge").len(), 1);
    let report = f.yard.reconcile_effects().unwrap();
    assert_eq!(report.settled, [(entry.id.clone(), EffectState::Confirmed)]);
    let lookup = &mock.calls_to("flaky__charge_lookup")[0];
    assert_eq!(lookup.arguments["key"], entry.id);
    assert_eq!(lookup.arguments["kind"], "charge");
    let settled = f.yard.effect(&entry.id).unwrap();
    assert_eq!(settled.state, EffectState::Confirmed);
    assert!(
        settled
            .detail
            .as_deref()
            .unwrap()
            .contains("flaky.charges.lookup"),
        "{settled:?}"
    );
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
    let draft = mail.staged.as_ref().unwrap().draft.as_ref().unwrap();
    assert_eq!(draft.handle, "d-1");
    assert_eq!(draft.draft_operation, "gmail.drafts.create");
    assert_eq!(draft.promote.as_ref().unwrap().tool, "gmail__drafts_send");
    assert_eq!(draft.discard.as_ref().unwrap().tool, "gmail__drafts_delete");
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

    // Promoting the draft makes the gateway's promote call, under the
    // entry's key; the original tool is not called again.
    let sent = f.yard.promote_effect(&mail.id, "ana", "cli").unwrap();
    assert_eq!(sent.state, EffectState::Confirmed);
    assert_eq!(sent.approval.as_ref().unwrap().by, "ana");
    assert_eq!(mock.calls_to("gmail__send").len(), 1);
    let promote = &mock.calls_to("gmail__drafts_send")[0];
    assert_eq!(promote.arguments, serde_json::json!({"id": "d-1"}));
    assert_eq!(promote.key.as_deref(), Some(mail.id.as_str()));
    assert_eq!(promote.meta["idempotency_key"], mail.id);
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
        rendered.contains(
            "  upstream, can be undone       slack: chat_post (undone by slack.chat.delete, until "
        ),
        "{rendered}"
    );
    assert!(
        rendered.contains(
            "  upstream, can be compensated  github: issues_create (compensated by github.issues.update)"
        ),
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
        .undo_effects_at(
            &plan,
            std::slice::from_ref(&issue.id),
            "ana",
            "cli",
            1_000_000,
        )
        .unwrap();
    assert_eq!(outcomes[0].state, EffectState::Compensated);
    // The compensation's tool, with its arguments, confirmed as its schema
    // requires.
    let close = &mock.calls_to("github__issues_close")[0];
    assert!(close.arguments["number"].is_u64());
    assert_eq!(close.arguments["state"], "closed");
    assert_eq!(close.arguments["confirm"], true);
    // Undoing the staged call discards it; it never happened.
    let staged = plan.staged[0].entry.clone();
    let outcomes = f
        .yard
        .undo_effects_at(
            &plan,
            std::slice::from_ref(&staged.id),
            "ana",
            "cli",
            1_000_000,
        )
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
    mock.without_compensation();
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
        .undo_effects_at(&before, std::slice::from_ref(&a), "ana", "cli", 6_000_000)
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
        .undo_effects(&plan, std::slice::from_ref(&id), "ana", "cli")
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

#[test]
fn past_its_deadline_an_inverse_gives_way_to_the_compensation() {
    let f = Fixture::new();
    let (mock, script) = setup(&f, ApprovalSettings::default());
    mock.set_deadline(5_000_000);
    f.task(&calls(
        &script,
        &[("slack__chat_post", r#"{"channel": "a"}"#)],
    ))
    .options(granted(&f))
    .name("late")
    .run()
    .unwrap();
    let after = f.yard.undo_plan_at("late", 0, 6_000_000).unwrap();
    assert_eq!(after.compensable.len(), 1, "{after:?}");
    assert!(after.reversible.is_empty());
    let note = &after.compensable[0].note;
    assert!(
        note.contains("expired") && note.contains("compensated by slack.chat.update"),
        "{note}"
    );
    // Not expired by reconciliation while the compensation works.
    let report = f.yard.reconcile_effects_at(6_000_000).unwrap();
    assert!(report.settled.is_empty(), "{report:?}");
    let id = after.compensable[0].entry.id.clone();
    let outcomes = f
        .yard
        .undo_effects_at(&after, std::slice::from_ref(&id), "ana", "cli", 6_000_000)
        .unwrap();
    assert_eq!(outcomes[0].state, EffectState::Compensated, "{outcomes:?}");
    assert!(mock.calls_to("slack__chat_delete").is_empty());
    let update = &mock.calls_to("slack__chat_update")[0];
    assert_eq!(
        update.arguments,
        serde_json::json!({"ts": "1", "text": "(retracted)"})
    );
}

#[test]
fn a_draft_is_discarded_upstream_and_one_without_a_discard_says_so() {
    let f = Fixture::new();
    let (mock, script) = setup(&f, ApprovalSettings::default());
    f.task(&calls(
        &script,
        &[
            ("gmail__send", r#"{"to": "finance@"}"#),
            ("blog__publish", r#"{"title": "launch"}"#),
        ],
    ))
    .options(granted(&f))
    .name("drafts")
    .run()
    .unwrap();
    let ledger = f.yard.effects(Some("drafts")).unwrap();
    let (mail, post) = (&ledger[0], &ledger[1]);
    assert_eq!(mail.state, EffectState::Staged);
    assert_eq!(post.state, EffectState::Staged);
    assert!(mock.calls_to("blog__publish")[0].staged());
    // Denying the mail's approval discards its draft with the gateway's
    // discard call.
    let ask = mail.staged.as_ref().unwrap().ask.clone();
    f.yard
        .answer_approval(&ask, false, "ana", "cli", Some("not yet"))
        .unwrap();
    let discard = &mock.calls_to("gmail__drafts_delete")[0];
    assert_eq!(discard.arguments, serde_json::json!({"id": "d-1"}));
    assert_eq!(
        discard.key.as_deref(),
        Some(format!("{}-discard", mail.id).as_str())
    );
    let denied = f.yard.effect(&mail.id).unwrap();
    assert_eq!(denied.state, EffectState::Failed);
    assert!(
        denied
            .detail
            .as_deref()
            .unwrap()
            .contains("its draft was discarded"),
        "{denied:?}"
    );
    // The post's draft has no discard: undo says so, and leaves it staged.
    let plan = f.yard.undo_plan("drafts", 0).unwrap();
    assert_eq!(plan.staged.len(), 1);
    assert!(
        plan.staged[0].note.contains("cannot be discarded"),
        "{:?}",
        plan.staged[0]
    );
    let outcomes = f
        .yard
        .undo_effects(&plan, std::slice::from_ref(&post.id), "ana", "cli")
        .unwrap();
    assert_eq!(outcomes[0].state, EffectState::Staged);
    assert!(
        outcomes[0].detail.contains("cannot be discarded"),
        "{outcomes:?}"
    );
    // Promoting it calls the promote tool with the draft's handle.
    let handle = post
        .staged
        .as_ref()
        .unwrap()
        .draft
        .as_ref()
        .unwrap()
        .handle
        .clone();
    let published = f.yard.promote_effect(&post.id, "ana", "cli").unwrap();
    assert_eq!(published.state, EffectState::Confirmed);
    let update = &mock.calls_to("blog__posts_update")[0];
    assert_eq!(
        update.arguments,
        serde_json::json!({"id": handle, "draft": false})
    );
    assert_eq!(mock.calls_to("blog__publish").len(), 1);
}

#[test]
fn a_rest_call_is_ledgered_like_an_mcp_call() {
    let f = Fixture::new();
    let (mock, script) = setup(&f, person(&[("legacy:*", Approval::Block)]));
    let rest =
        |tool: &str, arguments: &str| format!("SH python3 {script} --rest {tool} '{arguments}'");
    let branch = f
        .task(
            &[
                rest("slack__chat_post", r#"{"channel": "board"}"#),
                rest("github__issues_list", "{}"),
                rest("webhook__fire", r#"{"url": "https://x"}"#),
                rest("legacy__do", "{}"),
            ]
            .join("\n"),
        )
        .options(granted(&f))
        .name("rest")
        .run()
        .unwrap();
    let said = text(&branch.events().unwrap());
    assert!(
        said.contains("call slack__chat_post: \"posted ts=1\""),
        "{said}"
    );
    assert!(said.contains("call github__issues_list: []"), "{said}");
    assert!(
        said.contains("call webhook__fire: {\"staged\":true"),
        "{said}"
    );
    assert!(said.contains("call legacy__do error 403"), "{said}");
    assert!(said.contains("approval_blocked"), "{said}");
    let ledger = f.yard.effects(Some("rest")).unwrap();
    assert_eq!(ledger.len(), 2, "{ledger:?}");
    // Begun before the call, with the entry's id as the key; finished from
    // the X-Anvil-Effect header.
    let post = &ledger[0];
    assert_eq!(post.state, EffectState::Confirmed);
    assert_eq!(post.class, EffectClass::Reversible);
    assert_eq!(post.undo.as_ref().unwrap().tool, "slack__chat_delete");
    let call = &mock.calls_to("slack__chat_post")[0];
    assert!(call.rest);
    assert_eq!(call.key.as_deref(), Some(post.id.as_str()));
    // Held in the outbox: never sent.
    assert_eq!(ledger[1].state, EffectState::Staged);
    assert!(mock.calls_to("webhook__fire").is_empty());
    assert!(mock.calls_to("legacy__do").is_empty());
}

#[test]
fn the_audit_log_settles_a_lost_answer_and_records_calls_around_the_proxy() {
    let f = Fixture::new();
    let (mock, script) = setup(&f, person(&[("flaky:*", Approval::Allow)]));
    f.task(&calls(&script, &[("flaky__charge", r#"{"amount": 5}"#)]))
        .options(granted(&f))
        .name("audited")
        .run()
        .unwrap();
    let lost = f.yard.effects(Some("audited")).unwrap().remove(0);
    assert_eq!(lost.state, EffectState::Unknown);
    assert_eq!(mock.calls_to("flaky__charge").len(), 1);
    // The gateway's audit line for that call carries its key: it happened.
    // A second line is a call that did not go through the proxy, and says
    // its effect.
    let audit = f.root.join(".branchyard/gateway/audit.jsonl");
    let lines = format!(
        "{}\n{}\n",
        serde_json::json!({"time": "2026-10-03T10:00:00Z", "by_branch": "audited", "by_turn": "1",
            "sub": "local:me", "connector": "flaky", "operation": "flaky.charges.create",
            "decision": "allowed", "upstream_status": 200, "error_code": null,
            "effect_class": "compensable", "ledger_id": lost.id, "staged_for": null}),
        serde_json::json!({"time": "2026-10-03T10:00:01Z", "by_branch": "audited", "by_turn": "1",
            "sub": "local:me", "connector": "slack", "operation": "slack.chat.post",
            "decision": "allowed", "upstream_status": 200, "error_code": null,
            "effect_class": "reversible", "ledger_id": null, "staged_for": null}),
    );
    // A draft's line settles nothing and records nothing.
    let lines = format!(
        "{lines}{}\n",
        serde_json::json!({"time": "2026-10-03T10:00:02Z", "by_branch": "audited", "by_turn": "1",
            "sub": "local:me", "connector": "gmail", "operation": "gmail.drafts.create",
            "decision": "allowed", "upstream_status": 200, "error_code": null,
            "effect_class": "irreversible", "ledger_id": null, "staged_for": "gmail.send"})
    );
    fs::write(&audit, &lines).unwrap();
    f.yard.ingest_connector_audit().unwrap();
    let settled = f.yard.effect(&lost.id).unwrap();
    assert_eq!(settled.state, EffectState::Confirmed);
    assert!(
        settled.detail.as_deref().unwrap().contains("audit log"),
        "{settled:?}"
    );
    let ledger = f.yard.effects(Some("audited")).unwrap();
    assert_eq!(ledger.len(), 2, "{ledger:?}");
    let around = &ledger[1];
    assert_eq!(around.connector, "slack");
    assert_eq!(around.state, EffectState::Confirmed);
    assert_eq!(around.class, EffectClass::Reversible);
    assert_eq!(around.operation_id.as_deref(), Some("slack.chat.post"));
    assert_eq!(around.undo, None, "an audit line names no undo");
    assert_eq!(around.approval, None, "never approved");
    assert!(around
        .detail
        .as_deref()
        .unwrap()
        .contains("did not go through"));
    // Read again from the start (a replaced log): recorded once.
    fs::remove_file(&audit).unwrap();
    fs::write(&audit, &lines).unwrap();
    f.yard.ingest_connector_audit().unwrap();
    assert_eq!(f.yard.effects(Some("audited")).unwrap().len(), 2);
}
