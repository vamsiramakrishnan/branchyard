//! The engine driving Claude Code's stream-json profile against a shell
//! stand-in that prints the frames Claude Code 2.1.293 prints: how a
//! session is closed, a cost limit crossed by the turn's last message, the
//! cost of a turn while it runs, and the harness's private temporary
//! directory. No model is called.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::os::unix::fs::PermissionsExt as _;
use std::time::{Duration, Instant};

use branchyard::{Activity, BranchStatus, Budget, Policy, TaskOptions};
use branchyard_testkit::wait;
use common::Fixture;

/// Answers every control request it reads; `$1` is what it does once it
/// has the prompt. Like Claude Code with a background task, it does not
/// exit when its input closes, only when asked to end its session.
const STAND_IN: &str = r#"
reply() {
  id=$(printf '%s' "$1" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
  printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$id"
}
read -r line; reply "$line"
read -r prompt
echo '{"type":"system","subtype":"init","session_id":"stand-in-session"}'
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

fn result(cost: f64) -> String {
    format!(
        r#"echo '{{"type":"result","subtype":"success","is_error":false,"total_cost_usd":{cost},"modelUsage":{{"claude-sonnet-4-5":{{"inputTokens":10,"outputTokens":5}}}}}}'"#
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
