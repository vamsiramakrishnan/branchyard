//! The engine driving Claude Code's stream-json profile against a shell
//! stand-in that prints the frames Claude Code 2.1.293 prints: how a
//! session is closed, a cost limit crossed by the turn's last message, the
//! cost of a turn while it runs, the spending limit the harness is given
//! and stops at itself (none on the model gateway), the cost of a fresh
//! session after spending, and the harness's private temporary
//! directory. No
//! model is called.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::os::unix::fs::PermissionsExt as _;
use std::time::{Duration, Instant};

use branchyard::{
    models, Activity, BranchStatus, Budget, Policy, Provisioning, StallAction, TaskOptions,
};
use branchyard_testkit::wait;
use common::Fixture;
use serde_json::json;

/// Answers every control request it reads; `$1` is what it does once it
/// has the prompt. Like Claude Code with a background task, it does not
/// exit when its input closes, only when asked to end its session. It
/// continues the session `--resume` names, and otherwise starts a new one,
/// numbered in the branch's temporary directory.
const STAND_IN: &str = r#"
reply() {
  id=$(printf '%s' "$1" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
  printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$id"
}
session=; previous=
for a in "$@"; do [ "$previous" = --resume ] && session=$a; previous=$a; done
if [ -z "$session" ]; then
  n=$(( $(cat "$TMPDIR/sessions" 2>/dev/null || echo 0) + 1 ))
  printf '%s' "$n" > "$TMPDIR/sessions"
  session=stand-in-session-$n
fi
read -r line; reply "$line"
read -r prompt
printf '{"type":"system","subtype":"init","session_id":"%s"}\n' "$session"
echo '{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","rateLimitType":"five_hour"}}'
eval "$1"
echo '{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"b1","task_type":"local_bash","description":"sleep"}]}'
while read -r line; do
  case "$line" in
    *end_session*) reply "$line"; exit 0;;
    *control_request*) reply "$line";;
  esac
done
exec sleep 30
"#;

/// A turn's `result` after it spends `cost`. Like Claude Code, whose
/// `total_cost_usd` is the session's across `--resume` (2.1.293 reported
/// $0.0261, $0.0454 and $0.0649 for one session's three turns), it reports
/// the session's running total, kept in the branch's temporary directory
/// for each session. A session that is not resumed (no `--resume`) starts
/// its total again; one resumed continues its own.
fn result(cost: f64) -> String {
    format!(
        r#"before=$(cat "$TMPDIR/session-cost-$session" 2>/dev/null || echo 0)
total=$(awk -v spent={cost} -v before="$before" 'BEGIN {{ print before + spent }}')
printf '%s' "$total" > "$TMPDIR/session-cost-$session"
printf '{{"type":"result","subtype":"success","is_error":false,"total_cost_usd":%s,"modelUsage":{{"claude-sonnet-4-5":{{"inputTokens":10,"outputTokens":5}}}}}}\n' "$total""#
    )
}

fn stand_in(f: &Fixture, turn: &str) -> TaskOptions {
    TaskOptions {
        harness: Some("claude-code-stream-json".into()),
        command: Some(vec![
            "sh".into(),
            "-c".into(),
            STAND_IN.into(),
            "claude".into(),
            turn.into(),
        ]),
        policy: Policy::allow_all(),
        ..f.options()
    }
}

fn warnings(f: &Fixture, name: &str) -> Vec<String> {
    f.yard
        .branch(name)
        .unwrap()
        .events()
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.activity {
            Activity::Warning(text) => Some(text),
            _ => None,
        })
        .collect()
}

/// Claude Code waits for its background tasks before it exits; asked to
/// end its session it stops them and exits at once, and the close says
/// nothing.
#[test]
fn a_harness_with_background_work_is_closed_quietly() {
    let f = Fixture::new();
    let started = Instant::now();
    let branch = f
        .yard
        .task("go")
        .options(stand_in(&f, &result(0.01)))
        .name("quiet")
        .run()
        .unwrap();
    assert_eq!(branch.info().status, BranchStatus::NoChanges);
    assert_eq!(warnings(&f, "quiet"), Vec::<String>::new());
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "the close waited {:?}",
        started.elapsed()
    );
    // Its harness's own events are typed, not "unrecognized".
    let events = branch.events().unwrap();
    assert!(events.iter().any(|e| matches!(&e.activity,
        Activity::Harness(branchyard::Event::BackgroundTasks { running }) if running.len() == 1)));
    assert!(!events.iter().any(|e| matches!(
        &e.activity,
        Activity::Harness(branchyard::Event::Unrecognized { .. })
    )));
}

/// `by check` started in the background, as a child does before it ends
/// its turn ("I'll wait for its notification").
const CHECK_RUNNING: &str = r#"echo '{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"c1","task_type":"local_bash","description":"by check"}]}'
echo '{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"I will wait for the check to finish."}]}}'"#;

/// The harness's warnings, as `by events` shows them.
fn harness_warnings(f: &Fixture, name: &str) -> Vec<String> {
    f.yard
        .branch(name)
        .unwrap()
        .events()
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.activity {
            Activity::Harness(branchyard::Event::Warning { message }) => Some(message),
            _ => None,
        })
        .collect()
}

/// A child once ended its turn while `by check` ran in the background, and
/// the session's close killed the check. Now the turn is held open, quiet
/// as it is, without counting as a stall, until the task ends and Claude
/// Code has answered its notification; that answer is the turn's.
#[test]
fn a_turn_is_held_open_while_its_background_work_runs() {
    let f = Fixture::new();
    let turn = format!(
        r#"{CHECK_RUNNING}
{first}
sleep 1.5
echo '{{"type":"system","subtype":"task_notification","task_id":"c1","status":"completed"}}'
echo '{{"type":"system","subtype":"background_tasks_changed","tasks":[]}}'
echo '{{"type":"assistant","message":{{"id":"m2","content":[{{"type":"text","text":"The check passed."}}]}}}}'
{second}"#,
        first = result(0.01),
        second = result(0.02),
    );
    let options = TaskOptions {
        budget: Budget::default()
            .stall_after(Duration::from_millis(300))
            .stall_action(StallAction::Interrupt),
        ..stand_in(&f, &turn)
    };
    let started = Instant::now();
    let branch = f
        .yard
        .task("go")
        .options(options)
        .name("held")
        .run()
        .unwrap();
    assert!(started.elapsed() >= Duration::from_millis(1500));
    assert_eq!(branch.info().status, BranchStatus::NoChanges);
    let cost = branch.info().cost_usd.unwrap();
    assert!((cost - 0.03).abs() < 1e-9, "the follow-up's cost: {cost}");
    let events = branch.events().unwrap();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.activity, Activity::Stalled { .. })),
        "{events:?}"
    );
    let texts: Vec<&str> = events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Harness(branchyard::Event::MessageDelta { text, .. }) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        [
            "I will wait for the check to finish.",
            "\n\nThe check passed."
        ]
    );
    let ended = events
        .iter()
        .position(|e| {
            matches!(
                e.activity,
                Activity::Harness(branchyard::Event::TurnEnded { .. })
            )
        })
        .unwrap();
    let check_ended = events
        .iter()
        .position(|e| matches!(&e.activity,
            Activity::Harness(branchyard::Event::HarnessTaskEnded { task_id, .. }) if task_id == "c1"))
        .unwrap();
    assert!(check_ended < ended);
}

/// The turn's limits bound the hold: at `max_duration` the engine
/// interrupts, Claude Code answers, and the turn ends over its limit with
/// a warning naming the task still running.
#[test]
fn a_limit_ends_the_hold_with_a_warning_naming_the_running_tasks() {
    let f = Fixture::new();
    let turn = format!("{CHECK_RUNNING}\n{}", result(0.01));
    let options = TaskOptions {
        budget: Budget::default().duration(Duration::from_secs(1)),
        ..stand_in(&f, &turn)
    };
    let started = Instant::now();
    let branch = f
        .yard
        .task("go")
        .options(options)
        .name("cut")
        .run()
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        branch.info().status,
        BranchStatus::BudgetExceeded {
            limit: "max_duration".into()
        }
    );
    let cut: Vec<String> = harness_warnings(&f, "cut")
        .into_iter()
        .filter(|w| w.contains("ended with Claude Code's background tasks still running"))
        .collect();
    assert_eq!(cut.len(), 1, "{:?}", harness_warnings(&f, "cut"));
    // The stand-in reports its own `sleep` running once its turn's frames
    // are out, so that is the set the limit cut off.
    assert!(cut[0].contains("(b1)"), "{cut:?}");
}

/// The usage that crosses the cost limit arrives with the turn's end, in
/// one `result`: the turn ends over budget, with no failed interrupt and
/// no killed harness.
#[test]
fn a_limit_crossed_by_the_turns_last_message_needs_no_interrupt() {
    let f = Fixture::new();
    let options = TaskOptions {
        budget: Budget::usd(0.1),
        ..stand_in(&f, &result(0.5))
    };
    let branch = f
        .yard
        .task("go")
        .options(options)
        .name("over")
        .run()
        .unwrap();
    assert_eq!(
        branch.info().status,
        BranchStatus::BudgetExceeded {
            limit: "max_usd".into()
        }
    );
    assert_eq!(branch.info().cost_usd, Some(0.5));
    assert_eq!(warnings(&f, "over"), Vec::<String>::new());
}

/// While the turn runs, its cost is the price of the model calls so far,
/// written to the branch's record; the harness's own total replaces it at
/// the end.
#[test]
fn a_running_turns_cost_is_known_before_it_ends() {
    let f = Fixture::new();
    let go = f.dir.join("go");
    let turn = format!(
        r#"echo '{{"type":"assistant","message":{{"id":"m1","model":"claude-sonnet-4-5","content":[{{"type":"text","text":"working"}}],"usage":{{"input_tokens":1000,"output_tokens":2000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}}}}'
while [ ! -e '{go}' ]; do sleep 0.05; done
{result}"#,
        go = go.display(),
        result = result(0.25),
    );
    let options = stand_in(&f, &turn);
    let yard = f.yard.clone();
    let running = std::thread::spawn(move || {
        yard.task("go")
            .options(options)
            .name("live")
            .run()
            .map(|b| b.info().clone())
    });
    let live = wait::until("the running turn's cost", || {
        let info = f.yard.branch("live").ok()?.info().clone();
        (info.status == BranchStatus::Running)
            .then_some(info.cost_usd)
            .flatten()
    });
    // 1000 input and 2000 output tokens of Sonnet 4.5 by the catalog.
    let priced = branchyard::models::pricing::cost(
        branchyard::models::Api::Anthropic,
        "claude-sonnet-4-5",
        &branchyard::models::Tokens {
            input: 1000,
            output: 2000,
            ..Default::default()
        },
    )
    .unwrap();
    assert!((live - priced).abs() < 1e-9, "{live} vs {priced}");
    std::fs::write(&go, "").unwrap();
    let ended = running.join().unwrap().unwrap();
    assert_eq!(ended.cost_usd, Some(0.25), "the harness's figure wins");
}

/// A local harness gets a private temporary directory under the
/// branch's state, which goes when the branch does.
#[test]
fn every_branch_has_its_own_temporary_directory() {
    let f = Fixture::new();
    let branch = f.task("ENV TMPDIR TMP TEMP").name("tmp").run().unwrap();
    let dir = f.root.join(".branchyard/tmp/tmp");
    let said = common::text(&branch.events().unwrap());
    for name in ["TMPDIR", "TMP", "TEMP"] {
        assert!(
            said.contains(&format!("{name}={}\n", dir.display())),
            "{said}"
        );
    }
    let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    let other = f.task("ENV TMPDIR").name("other").run().unwrap();
    assert!(common::text(&other.events().unwrap()).contains("/.branchyard/tmp/other\n"));
    f.yard.remove("tmp").unwrap();
    assert!(!dir.exists());
}

/// A child spawned with a model runs it: the engine opens its session with
/// it and the stream-json driver passes it as `--model`. Its record and
/// its inspection say which; its parent, spawned without one, runs the
/// harness's default.
#[test]
fn a_child_spawned_with_a_model_runs_it() {
    let f = Fixture::new();
    let turn = format!("printf '%s\\n' \"$@\" > args.txt\n{}", result(0.01));
    let options = TaskOptions {
        delegation: Some(branchyard::Envelope::default()),
        delegation_server: Some(vec![common::fake_agent().display().to_string()]),
        ..stand_in(&f, &turn)
    };
    let root = f
        .yard
        .task("go")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let args = |name: &str| {
        let worktree = f.yard.branch(name).unwrap().info().worktree.clone();
        std::fs::read_to_string(worktree.join("args.txt")).unwrap()
    };
    assert!(!args("root").contains("--model"), "{}", args("root"));
    let delegate = root.delegate(options).unwrap();
    let kid = delegate
        .spawn(branchyard::Spawn {
            name: Some("kid".into()),
            model: Some("claude-haiku-5-5".into()),
            ..branchyard::Spawn::new("go")
        })
        .unwrap();
    assert_eq!(kid.model.as_deref(), Some("claude-haiku-5-5"));
    root.wait_subtree().unwrap();
    assert!(
        args("kid").contains("--model\nclaude-haiku-5-5\n"),
        "{}",
        args("kid")
    );
    let info = f.yard.branch("kid").unwrap().info().clone();
    assert_eq!(info.model.as_deref(), Some("claude-haiku-5-5"));
    let inspected = delegate.inspect("kid").unwrap();
    assert_eq!(inspected.model.as_deref(), Some("claude-haiku-5-5"));
    assert_eq!(f.yard.branch("root").unwrap().info().model, None);
}

/// The `--max-budget-usd` a turn's stand-in was started with, if any.
fn harness_budget(f: &Fixture, name: &str) -> Option<f64> {
    let worktree = f.yard.branch(name).unwrap().info().worktree.clone();
    let args = std::fs::read_to_string(worktree.join("args.txt")).unwrap();
    let mut args = args.lines();
    args.by_ref().find(|a| *a == "--max-budget-usd")?;
    args.next()?.parse().ok()
}

/// Claude Code is given what is left of the branch's budget for each turn,
/// its limit less what it has spent, and nothing without a limit. Its
/// `--max-budget-usd` counts only its own process's spend while the cost
/// it reports is the session's (2.1.293: a resumed turn given $0.0209
/// succeeded at a cumulative $0.0649), so a resumed turn is given the
/// limit less the earlier turns' spend, and the branch's cost is the
/// session's total.
#[test]
fn the_harness_is_given_what_is_left_of_the_budget() {
    let f = Fixture::new();
    let args = "printf '%s\\n' \"$@\" > args.txt\n";
    let turn = format!("{args}{}", result(0.04));
    let branch = f
        .yard
        .task("go")
        .options(TaskOptions {
            budget: Budget::usd(0.1),
            ..stand_in(&f, &turn)
        })
        .name("capped")
        .run()
        .unwrap();
    assert_eq!(harness_budget(&f, "capped"), Some(0.1));
    assert_eq!(branch.info().cost_usd, Some(0.04));
    let again = branch
        .send(
            "again",
            TaskOptions {
                budget: Budget::usd(0.1),
                ..stand_in(&f, &format!("{args}{}", result(0.03)))
            },
        )
        .unwrap();
    let worktree = &again.info().worktree;
    let argv = std::fs::read_to_string(worktree.join("args.txt")).unwrap();
    assert!(argv.lines().any(|a| a == "--resume"), "{argv}");
    let left = harness_budget(&f, "capped").unwrap();
    assert!((left - 0.06).abs() < 1e-9, "{left}");
    let spent = again.info().cost_usd.unwrap();
    assert!((spent - 0.07).abs() < 1e-9, "the session's total: {spent}");

    f.yard
        .task("go")
        .options(stand_in(&f, &turn))
        .name("unlimited")
        .run()
        .unwrap();
    assert_eq!(harness_budget(&f, "unlimited"), None);
}

/// A fresh session's total starts again from nothing (here after a rewind
/// to the base), so the branch's cost is what it had spent before plus the
/// session's total: its recorded cost does not fall, and the next turn is
/// not given more than is left.
#[test]
fn a_fresh_session_adds_to_what_the_branch_spent() {
    let f = Fixture::new();
    let args = "printf '%s\\n' \"$@\" > args.txt\n";
    let options = |cost: f64| TaskOptions {
        budget: Budget::usd(0.1),
        ..stand_in(&f, &format!("{args}{}", result(cost)))
    };
    let branch = f
        .yard
        .task("go")
        .options(options(0.04))
        .name("refreshed")
        .run()
        .unwrap();
    assert_eq!(branch.info().cost_usd, Some(0.04));
    branch.rewind(0).unwrap();
    let fresh = branch.send("again", options(0.03)).unwrap();
    let argv = std::fs::read_to_string(fresh.info().worktree.join("args.txt")).unwrap();
    assert!(!argv.lines().any(|a| a == "--resume"), "{argv}");
    let spent = fresh.info().cost_usd.unwrap();
    assert!(
        (spent - 0.07).abs() < 1e-9,
        "before plus the session's: {spent}"
    );

    // The fresh session resumed: its total counts both its turns.
    let next = fresh.send("more", options(0.01)).unwrap();
    let argv = std::fs::read_to_string(next.info().worktree.join("args.txt")).unwrap();
    assert!(argv.lines().any(|a| a == "--resume"), "{argv}");
    let left = harness_budget(&f, "refreshed").unwrap();
    assert!((left - 0.03).abs() < 1e-9, "{left}");
    let spent = next.info().cost_usd.unwrap();
    assert!((spent - 0.08).abs() < 1e-9, "{spent}");
}

/// A rewind can resume an older session, whose total is lower than the
/// latest one's: the turn counts only what it adds to that session's own
/// total, so the branch's recorded cost does not fall.
#[test]
fn resuming_an_older_session_adds_to_what_the_branch_spent() {
    let f = Fixture::new();
    let args = "printf '%s\\n' \"$@\" > args.txt\n";
    let options = |cost: f64| stand_in(&f, &format!("{args}{}", result(cost)));
    let branch = f
        .yard
        .task("go")
        .options(options(0.01))
        .name("older")
        .run()
        .unwrap();
    let first = branch.info().session.clone().unwrap();
    branch.rewind(0).unwrap();
    let fresh = branch.send("again", options(0.10)).unwrap();
    assert_ne!(fresh.info().session.as_deref(), Some(first.as_str()));
    let spent = fresh.info().cost_usd.unwrap();
    assert!((spent - 0.11).abs() < 1e-9, "{spent}");

    // Back to turn 1, which ended the first session: it resumes, at its
    // own total of $0.01.
    branch.rewind(1).unwrap();
    let resumed = branch.send("more", options(0.01)).unwrap();
    let argv = std::fs::read_to_string(resumed.info().worktree.join("args.txt")).unwrap();
    assert!(argv.contains(&format!("--resume\n{first}\n")), "{argv}");
    let spent = resumed.info().cost_usd.unwrap();
    assert!((spent - 0.12).abs() < 1e-9, "the cost fell: {spent}");

    // And the latest session, resumed again, still adds only its own.
    let next = resumed.send("and more", options(0.02)).unwrap();
    let spent = next.info().cost_usd.unwrap();
    assert!((spent - 0.14).abs() < 1e-9, "{spent}");
}

/// A turn cut off before its harness reported a total leaves the branch's
/// cost at its live estimate, which the session's own total counts too:
/// the turn that resumes the session counts that spending once.
#[test]
fn a_cut_off_turns_spending_is_counted_once_when_its_session_resumes() {
    let f = Fixture::new();
    let args = "printf '%s\\n' \"$@\" > args.txt\n";
    let options = |turn: &str| stand_in(&f, &format!("{args}{turn}"));
    let branch = f
        .yard
        .task("go")
        .options(options(&result(0.01)))
        .name("cut")
        .run()
        .unwrap();
    let session = branch.info().session.clone().unwrap();
    // 1000 input and 2000 output tokens of Sonnet 4.5, $0.033 by the
    // catalog, then the harness exits with no result; its session's total
    // counts them.
    let cut_off = r#"echo '{"type":"assistant","message":{"id":"m1","model":"claude-sonnet-4-5","content":[{"type":"text","text":"working"}],"usage":{"input_tokens":1000,"output_tokens":2000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}'
before=$(cat "$TMPDIR/session-cost-$session")
awk -v before="$before" 'BEGIN { print before + 0.033 }' > "$TMPDIR/session-cost-$session"
exit 3"#;
    let cut = branch.send("more", options(cut_off)).unwrap();
    assert_ne!(cut.info().status, BranchStatus::Ready);
    let live = cut.info().cost_usd.unwrap();
    assert!((live - 0.043).abs() < 1e-9, "the live estimate: {live}");

    let next = branch.send("again", options(&result(0.01))).unwrap();
    let argv = std::fs::read_to_string(next.info().worktree.join("args.txt")).unwrap();
    assert!(argv.contains(&format!("--resume\n{session}\n")), "{argv}");
    let spent = next.info().cost_usd.unwrap();
    assert!((spent - 0.053).abs() < 1e-9, "counted twice: {spent}");
}

/// A branch on the model gateway is given no spending limit of its own:
/// the gateway refuses an over-budget call itself, and its metered cost,
/// not the harness's, is the branch's.
#[test]
fn a_harness_on_the_model_gateway_is_given_no_limit() {
    let f = Fixture::new();
    std::fs::write(f.dir.join("anthropic"), "sk-real-anthropic-1\n").unwrap();
    let config: models::Config = serde_json::from_value(json!({
        "backends": {
            "anthropic": {"api": "anthropic", "url": "http://127.0.0.1:9", "key": "anthropic"}
        },
        "routes": [{"model": "claude-*", "backends": ["anthropic"]}]
    }))
    .unwrap();
    let signer = models::Signer::local(&f.yard).unwrap();
    let dir = f.dir.clone();
    f.yard.use_models(
        models::Gateway::new(&config, signer, move |name| {
            models::KeySource::File(dir.join(name))
        })
        .unwrap(),
    );
    let turn = format!(
        "printf '%s\\n' \"$@\" > args.txt\nprintf '%s' \"$ANTHROPIC_BASE_URL\" > base.txt\n{}",
        result(0.04)
    );
    let branch = f
        .yard
        .task("go")
        .options(TaskOptions {
            budget: Budget::usd(0.1),
            provision: Some(Provisioning {
                models: Some(models::ModelAccess {
                    allow: vec!["claude-*".into()],
                }),
                ..Provisioning::default()
            }),
            ..stand_in(&f, &turn)
        })
        .name("metered")
        .run()
        .unwrap();
    assert_eq!(branch.info().status, BranchStatus::Ready);
    let base = std::fs::read_to_string(branch.info().worktree.join("base.txt")).unwrap();
    assert!(base.ends_with("/anthropic"), "on the gateway: {base:?}");
    assert_eq!(harness_budget(&f, "metered"), None);
}

/// A harness that stops itself at the limit it was given ends the turn
/// over the branch's budget, and the log says the harness stopped it,
/// whether or not the cost it last reported crossed the limit too.
#[test]
fn a_harness_stopping_at_its_own_limit_is_recorded() {
    let f = Fixture::new();
    for (name, cost) in [("under", 0.09), ("over", 0.12)] {
        let turn = format!(
            r#"echo '{{"type":"result","subtype":"error_max_budget_usd","is_error":true,"total_cost_usd":{cost}}}'"#
        );
        let branch = f
            .yard
            .task("go")
            .options(TaskOptions {
                budget: Budget::usd(0.1),
                ..stand_in(&f, &turn)
            })
            .name(name)
            .run()
            .unwrap();
        assert_eq!(
            branch.info().status,
            BranchStatus::BudgetExceeded {
                limit: "max_usd".into()
            },
            "{name}"
        );
        assert_eq!(branch.info().cost_usd, Some(cost));
        assert_eq!(
            warnings(&f, name),
            vec![
                "the harness stopped itself at the $0.1000 spending limit it was given, \
                 what was left of max_usd (--budget-usd) when the turn started"
                    .to_owned()
            ],
            "{name}"
        );
    }
}
