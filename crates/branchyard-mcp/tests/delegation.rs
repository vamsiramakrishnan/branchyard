//! A harness delegating through the real `branchyard-mcp`: the fake ACP
//! agent starts the MCP server the engine projected into its session, speaks
//! MCP to it, and reports each tool result in its reply.

mod common;

use std::fs;
use std::time::{Duration, Instant};

use branchyard::{
    Activity, BranchStatus, Budget, ChildBudget, Envelope, Provisioning, Seat, Seats, TaskOptions,
};
use common::{text, Client, Fixture};
use serde_json::{json, Value};

/// The agent's reply to the parent's turn.
fn reply(f: &Fixture, branch: &str) -> String {
    text(&f.yard.branch(branch).unwrap().events().unwrap())
}

/// The JSON a tool returned, from `mcp <tool>: <json>` in a reply.
fn result(reply: &str, tool: &str) -> Value {
    let prefix = format!("mcp {tool}: ");
    let start = reply
        .find(&prefix)
        .unwrap_or_else(|| panic!("no {tool} result in {reply}"))
        + prefix.len();
    let mut stream = serde_json::Deserializer::from_str(&reply[start..]).into_iter::<Value>();
    stream.next().unwrap().unwrap()
}

#[test]
fn a_harness_spawns_a_child_and_integrates_it_into_its_own_branch() {
    let f = Fixture::new();
    let options = TaskOptions {
        check: Some(vec!["test".into(), "-f".into(), "child.txt".into()]),
        ..f.delegating(Envelope::default())
    };
    let main_before = f.git(&["rev-parse", "main"]);
    let prompt = [
        "MCP tools",
        r#"MCP spawn {"prompt": "WRITE child.txt=hello", "name": "kid"}"#,
        "MCP wait kid",
        r#"MCP send {"branch": "kid", "prompt": "WRITE more.txt=again"}"#,
        "MCP wait kid",
        r#"MCP events {"branch": "kid", "cursor": 0, "limit": 3}"#,
        r#"MCP propose_integration {"branch": "kid"}"#,
        "MCP children",
    ]
    .join("\n");
    let root = f
        .yard
        .task(prompt)
        .options(options)
        .name("root")
        .run()
        .unwrap();
    let said = reply(&f, "root");
    assert!(
        said.contains(&format!("mcp tools: {}", branchyard_mcp::TOOLS.join(","))),
        "{said}"
    );
    let spawned = result(&said, "spawn");
    assert_eq!(spawned["name"], "kid");
    assert_eq!(spawned["status"], json!({"state": "running"}));
    assert_eq!(spawned["depth"], 1);
    assert!(said.contains("mcp wait: ready"), "{said}");
    assert_eq!(result(&said, "send")["name"], "kid");
    let page = result(&said, "events");
    assert_eq!(page["events"].as_array().unwrap().len(), 3);
    assert_eq!(page["next_cursor"], 3);
    let merged = result(&said, "propose_integration");
    assert_eq!(merged["target"], "by/root");
    let children = result(&said, "children");
    assert_eq!(children["descendants"][0]["name"], "kid");

    // The child's work is in the parent's candidate, not in main.
    assert_eq!(root.info().status, BranchStatus::Ready);
    let candidate = root.info().candidate.as_ref().unwrap();
    assert_eq!(candidate.commit, merged["commit"].as_str().unwrap());
    assert_eq!(candidate.files_changed, 2);
    let diff = root.diff().unwrap();
    assert!(diff.contains("+hello") && diff.contains("+again"), "{diff}");
    assert_eq!(f.git(&["rev-parse", "main"]), main_before);
    let kid = f.yard.branch("kid").unwrap();
    assert_eq!(kid.info().parent.as_deref(), Some("root"));
    assert_eq!(kid.info().turns, 2);
    assert!(matches!(
        &kid.info().status,
        BranchStatus::Merged { target, .. } if target == "by/root"
    ));
    // The parent's log records each operation it asked for.
    let asked: Vec<(String, bool)> = root
        .events()
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.activity {
            Activity::Delegation { tool, refused, .. } => Some((tool, refused)),
            _ => None,
        })
        .collect();
    assert_eq!(
        asked,
        [
            ("spawn".into(), false),
            ("send".into(), false),
            ("integrate".into(), false)
        ]
    );
    // The token and the broker are gone with the turn.
    assert!(!f.root.join(".branchyard/delegation/root.json").exists());
    assert!(root.wait_subtree().unwrap().len() == 1);
}

#[test]
fn refusals_reach_the_harness_as_tool_errors() {
    let f = Fixture::new();
    f.yard
        .task("say other")
        .options(f.delegating(Envelope::default()))
        .name("other")
        .run()
        .unwrap();
    let options = TaskOptions {
        budget: Budget::usd(1.0),
        ..f.delegating(Envelope::default())
    };
    let prompt = [
        r#"MCP inspect {"branch": "other"}"#,
        r#"MCP cancel {"branch": "root"}"#,
        r#"MCP spawn {"prompt": "x", "harness": "qwen-code", "budget": {"max_usd": 0.1}}"#,
        r#"MCP spawn {"prompt": "x"}"#,
        r#"MCP spawn {"prompt": "x", "budget": {"max_usd": 2}}"#,
        r#"MCP spawn {"prompt": "x", "bogus": 1}"#,
        r#"MCP spawn {"prompt": "MCP spawn {\"prompt\": \"y\"}", "name": "kid", "budget": {"max_usd": 0.5}}"#,
        "MCP wait kid",
        "MCP inspect {}",
    ]
    .join("\n");
    let root = f
        .yard
        .task(prompt)
        .options(options)
        .name("root")
        .run()
        .unwrap();
    root.wait_subtree().unwrap();
    let said = reply(&f, "root");
    let errors: Vec<&str> = said
        .lines()
        .filter(|line| line.contains(" error: "))
        .collect();
    assert_eq!(errors.len(), 6, "{said}");
    assert!(
        errors[0].contains("other is not a descendant of root"),
        "{said}"
    );
    assert!(errors[1].contains("only on its descendants"), "{said}");
    assert!(
        errors[2].contains("may not delegate to qwen-code-acp"),
        "{said}"
    );
    assert!(
        errors[3].contains("needs max_usd; $1.0000 remains"),
        "{said}"
    );
    assert!(errors[4].contains("exceeds what root has left"), "{said}");
    assert!(errors[5].contains("unknown field `bogus`"), "{said}");
    // The child was one level down with no envelope left: its harness got
    // no MCP server at all.
    assert_eq!(reply(&f, "kid"), "mcp: no server\n");
    let me = result(&said, "inspect");
    assert_eq!(me["name"], "root");
    assert_eq!(me["max_usd"], 1.0);
    assert_eq!(me["remaining_usd"], 0.5);
    // The other branch was never touched.
    assert!(f.yard.branch("other").unwrap().info().children.is_empty());
}

#[test]
fn steer_adds_to_a_running_childs_turn() {
    let f = Fixture::new();
    let prompt = [
        r#"MCP spawn {"prompt": "AWAIT_STEER", "name": "kid"}"#,
        "MCP started kid",
        r#"MCP steer {"branch": "kid", "text": "also update the docs"}"#,
        "MCP wait kid",
        r#"MCP steer {"branch": "kid", "text": "too late"}"#,
    ]
    .join("\n");
    let root = f
        .yard
        .task(prompt)
        .options(f.delegating(Envelope::default()))
        .name("root")
        .run()
        .unwrap();
    root.wait_subtree().unwrap();
    let said = reply(&f, "root");
    let steered = result(&said, "steer");
    assert_eq!(steered["branch"], "kid");
    assert_eq!(steered["by"], "root");
    assert!(
        matches!(
            steered["state"]["state"].as_str(),
            Some("delivered" | "accepted")
        ),
        "{said}"
    );
    assert!(
        said.contains("mcp steer error: branch kid is not running a turn"),
        "{said}"
    );
    let kid = f.yard.branch("kid").unwrap();
    assert_eq!(kid.info().status, BranchStatus::NoChanges);
    assert!(text(&kid.events().unwrap()).contains("steered: also update the docs"));
}

#[test]
fn cancel_stops_a_running_child_and_its_subtree() {
    let f = Fixture::new();
    let prompt = [
        r#"MCP spawn {"prompt": "HANG", "name": "kid"}"#,
        "MCP started kid",
        r#"MCP cancel {"branch": "kid"}"#,
        "MCP wait kid",
        r#"MCP send {"branch": "kid", "prompt": "say again"}"#,
        "MCP wait kid",
        r#"MCP inspect {"branch": "kid"}"#,
    ]
    .join("\n");
    f.yard
        .task(prompt)
        .options(f.delegating(Envelope::default()))
        .name("root")
        .run()
        .unwrap();
    let said = reply(&f, "root");
    assert_eq!(result(&said, "cancel"), json!({"cancelled": ["kid"]}));
    assert!(said.contains("mcp wait: interrupted"), "{said}");
    // A cancelled child can be continued.
    assert_eq!(
        result(&said, "inspect")["last_message"],
        "echo: say again",
        "{said}"
    );
}

#[test]
fn the_server_refuses_a_forged_token_while_the_real_one_works() {
    let f = Fixture::new();
    let yard = f.yard.clone();
    let options = f.delegating(Envelope::default());
    let running = std::thread::spawn(move || {
        yard.task("HANG")
            .options(options)
            .name("root")
            .run()
            .unwrap()
    });
    let token_file = f.root.join(".branchyard/delegation/root.json");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !token_file.is_file() {
        assert!(Instant::now() < deadline, "no token file");
        std::thread::sleep(Duration::from_millis(20));
    }
    let token: Value = serde_json::from_str(&fs::read_to_string(&token_file).unwrap()).unwrap();
    let token = token["token"].as_str().unwrap();

    let mut forged = Client::start(&f.root, "root", "0000forged");
    forged.initialize();
    let (error, said) = forged.call("spawn", json!({"prompt": "x"}));
    assert!(error);
    assert!(said.contains("no running turn"), "{said}");
    let mut claimed = Client::start(&f.root, "other", token);
    claimed.initialize();
    let (error, said) = claimed.call("inspect", json!({}));
    assert!(
        error && said.contains("issued to root, not other"),
        "{said}"
    );
    let mut real = Client::start(&f.root, "root", token);
    real.initialize();
    let (error, said) = real.call("inspect", json!({}));
    assert!(!error, "{said}");
    assert_eq!(
        serde_json::from_str::<Value>(&said).unwrap()["name"],
        "root"
    );
    assert!(f.yard.branches().unwrap().len() == 1, "nothing was spawned");

    f.yard.branch("root").unwrap().cancel().unwrap();
    assert_eq!(
        running.join().unwrap().info().status,
        BranchStatus::Interrupted
    );
    // Revoked with the turn.
    let (error, said) = real.call("inspect", json!({}));
    assert!(error, "{said}");
}

#[test]
fn a_harness_in_a_rig_spawns_by_seat_over_mcp() {
    let f = Fixture::new();
    let worker = Seat {
        harness: "gemini-cli".into(),
        budget: ChildBudget::default(),
        check: None,
        deny: Vec::new(),
        isolated: false,
        provision: Some(Provisioning {
            instructions: Some("You write files.".into()),
            ..Provisioning::default()
        }),
        delegates_to: Vec::new(),
        instances: 1,
    };
    let seats = Seats {
        rig: "team".into(),
        seat: "lead".into(),
        delegates_to: vec!["worker".into()],
        table: [("worker".to_owned(), worker)].into_iter().collect(),
    };
    let options = TaskOptions {
        seats: Some(seats.clone()),
        ..f.delegating(seats.envelope())
    };
    let prompt = [
        "MCP inspect {}",
        r#"MCP spawn {"prompt": "x"}"#,
        r#"MCP spawn {"prompt": "x", "seat": "boss"}"#,
        // Escaped, so the agent reads the keyword only in the child's prompt.
        r#"MCP spawn {"prompt": "\u0049NSTRUCTED", "seat": "worker"}"#,
        "MCP wait root-worker",
        r#"MCP propose_integration {"branch": "root-worker"}"#,
    ]
    .join("\n");
    let root = f
        .yard
        .task(prompt)
        .options(options)
        .name("root")
        .run()
        .unwrap();
    root.wait_subtree().unwrap();
    let said = reply(&f, "root");
    let me = result(&said, "inspect");
    assert_eq!(me["seat"], "lead");
    assert_eq!(me["seats"], json!(["worker"]));
    let errors: Vec<&str> = said
        .lines()
        .filter(|line| line.contains(" error: "))
        .collect();
    assert_eq!(errors.len(), 3, "{said}");
    assert!(
        errors[0].contains("spawns only by seat: one of worker"),
        "{said}"
    );
    assert!(
        errors[1].contains("may spawn only worker, not boss"),
        "{said}"
    );
    // Nothing to merge: the worker changed no file.
    assert!(errors[2].contains("root-worker"), "{said}");
    let spawned = result(&said, "spawn");
    assert_eq!(spawned["name"], "root-worker");
    assert_eq!(spawned["seat"], "worker");
    assert_eq!(reply(&f, "root-worker"), "instructed=true");
}

#[test]
fn artifacts_and_scratch_areas_are_reachable_over_mcp() {
    let f = Fixture::new();
    let prompt = [
        r#"MCP publish_artifact {"path": "a.txt", "name": "payload.txt"}"#,
        r#"MCP create_scratch {"name": "shared-cache"}"#,
        r#"MCP spawn {"prompt": "MCP list_artifacts\nMCP list_scratch\nMCP lock_scratch {\"name\": \"shared-cache\"}", "name": "kid", "max_depth": 1}"#,
        "MCP wait kid",
    ]
    .join("\n");
    f.yard
        .task(prompt)
        .options(f.delegating(Envelope::depth(2)))
        .name("root")
        .run()
        .unwrap();
    let said = reply(&f, "root");
    let published = result(&said, "publish_artifact");
    assert_eq!(published["name"], "payload.txt");
    assert!(!published["digest"].as_str().unwrap().is_empty());
    let area = result(&said, "create_scratch");
    assert_eq!(area["name"], "shared-cache");

    // The child, a descendant of root, reads what root published and
    // created without any explicit share.
    let kid_said = reply(&f, "kid");
    let listed = result(&kid_said, "list_artifacts");
    let names: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["payload.txt"]);
    let scratch = result(&kid_said, "list_scratch");
    assert_eq!(scratch[0]["name"], "shared-cache");
    let lock = result(&kid_said, "lock_scratch");
    assert_eq!(lock["holder_branch"], "kid");
}
