//! Wave 2's developer conveniences through the built `by`, hermetically:
//! quota meters read from fixture session files (`by usage`, the guard on
//! `by run`), issues from Linear, Jira and GitLab served by local mock HTTP
//! servers and through a mock connector gateway (`--issue`), a branch
//! started from a pull request's head with a fake `gh` (`--pr`), listening
//! ports attributed to branches with real local listeners (`by workspace
//! ports|browse|kill`, concurrent `run --detach`), and `by adopt` of
//! fixture Claude Code and Codex sessions resumed by the fake ACP agent.
//! Nothing reaches a real service and no real credential is read: every
//! test points HOME, CLAUDE_CONFIG_DIR and CODEX_HOME at fixtures.
//! Requires `git` and `sh`; the port tests also `python3`, and say so and
//! pass without it.

#![allow(
    clippy::let_underscore_must_use,
    clippy::map_unwrap_or,
    clippy::unwrap_used
)] // tests: a panic is the failure report
use std::collections::BTreeMap;
use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use branchyard_support::time::{now_ms, rfc3339};

use branchyard_testkit::fake_agent;
use branchyard_testkit::wait;
use branchyard_testkit::{MockHttp, Request, Response};
use serde_json::{json, Value};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Repo {
    dir: PathBuf,
    root: PathBuf,
    home: PathBuf,
    env: BTreeMap<String, String>,
}

impl Repo {
    fn new(project: &str) -> Repo {
        let dir = std::env::temp_dir().join(format!(
            "branchyard-cli-devex-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("repo")).unwrap();
        fs::create_dir_all(dir.join("home")).unwrap();
        let dir = fs::canonicalize(dir).unwrap();
        let repo = Repo {
            root: dir.join("repo"),
            home: dir.join("home"),
            dir,
            env: BTreeMap::new(),
        };
        repo.git(&["init", "-q", "-b", "main"]);
        repo.git(&["config", "user.name", "Test"]);
        repo.git(&["config", "user.email", "test@localhost"]);
        fs::write(repo.root.join("a.txt"), "one\n").unwrap();
        fs::write(repo.root.join(".gitignore"), "branchyard.toml\n").unwrap();
        repo.git(&["add", "."]);
        repo.git(&["commit", "-q", "-m", "initial"]);
        if !project.is_empty() {
            fs::write(repo.root.join("branchyard.toml"), project).unwrap();
        }
        repo
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(&self.root)
            .env("HOME", &self.home)
            .env("CLAUDE_CONFIG_DIR", self.home.join(".claude"))
            .env("CODEX_HOME", self.home.join(".codex"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("NO_COLOR", "1")
            .env("BRANCHYARD_USER_CONFIG", self.dir.join("user/config.toml"))
            .env("BRANCHYARD_TRUST_FILE", self.dir.join("user/trust.json"))
            .env("PAGER", "cat");
        for var in [
            "BRANCHYARD_DELEGATION",
            "BRANCHYARD_BRANCH",
            "BRANCHYARD_ROOT",
            "BRANCHYARD_BY",
            "BRANCHYARD_REMOTE",
            "LINEAR_API_KEY",
            "LINEAR_ACCESS_TOKEN",
            "LINEAR_API_URL",
            "JIRA_EMAIL",
            "JIRA_API_TOKEN",
            "JIRA_URL",
            "GITLAB_TOKEN",
            "GITLAB_URL",
            "BROWSER",
        ] {
            command.env_remove(var);
        }
        command.envs(&self.env);
        command
    }

    fn git(&self, args: &[&str]) -> String {
        let out = self.command("git").args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", stderr(&out));
        String::from_utf8(out.stdout).unwrap()
    }

    fn by(&self, args: &[&str]) -> Output {
        self.command(env!("CARGO_BIN_EXE_by"))
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.by(args);
        assert!(out.status.success(), "by {args:?}: {}", stderr(&out));
        stdout(&out)
    }

    fn json(&self, args: &[&str]) -> Value {
        serde_json::from_str(&self.ok(args)).unwrap()
    }

    /// A branch run by the fake agent, as `gemini-cli`.
    fn agent(&self, args: &[&str]) -> Output {
        let agent = fake_agent!().display().to_string();
        let mut all: Vec<&str> = args.to_vec();
        all.extend(["--command", &agent, "--yes", "--harness", "gemini-cli"]);
        self.by(&all)
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn write_lines(path: &Path, lines: &[Value]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    fs::write(path, text).unwrap();
}

// Quota meters.

/// A Claude Code assistant record with usage.
fn claude_turn(at: u64, id: &str, input: u64, output: u64) -> Value {
    json!({
        "type": "assistant", "sessionId": "s1", "timestamp": rfc3339(at), "cwd": "/w",
        "requestId": format!("req-{id}"), "uuid": format!("u-{id}"),
        "message": {"id": format!("msg-{id}"), "model": "claude-opus-4-8",
                    "usage": {"input_tokens": input, "output_tokens": output,
                              "cache_read_input_tokens": 1000, "cache_creation_input_tokens": 0}}
    })
}

fn codex_rollout(at: u64, primary: f64, secondary: f64) -> Vec<Value> {
    let reset = |hours: u64| (at / 1000) + hours * 3600;
    vec![
        json!({"timestamp": rfc3339(at), "type": "session_meta",
               "payload": {"id": "c1", "cwd": "/w", "source": "cli"}}),
        json!({"timestamp": rfc3339(at), "type": "turn_context",
               "payload": {"cwd": "/w", "model": "gpt-5.5"}}),
        json!({"timestamp": rfc3339(at), "type": "event_msg",
               "payload": {"type": "token_count",
                           "info": {"total_token_usage": {"input_tokens": 1000, "cached_input_tokens": 0, "output_tokens": 100, "total_tokens": 1100},
                                    "last_token_usage": {"input_tokens": 1000, "cached_input_tokens": 0, "output_tokens": 100, "total_tokens": 1100}},
                           "rate_limits": {
                               "primary": {"used_percent": primary, "window_minutes": 300, "resets_at": reset(2)},
                               "secondary": {"used_percent": secondary, "window_minutes": 10080, "resets_at": reset(72)}}}}),
    ]
}

#[test]
fn usage_reads_both_logins_and_the_guard_warns_or_refuses() {
    let repo = Repo::new("");
    let now = now_ms();
    let hour = 3_600_000;
    let claude = repo.home.join(".claude/projects/-w");
    // Two hours ago (the open 5-hour window), streamed twice; and five
    // days ago (the week only).
    write_lines(
        &claude.join("s1.jsonl"),
        &[
            claude_turn(now - 2 * hour, "a", 100, 10),
            claude_turn(now - 2 * hour, "a", 100, 900),
            claude_turn(now - 2 * hour + 60_000, "b", 1000, 0),
            json!({"type": "user", "message": {"role": "user", "content": "assistant, hi"}}),
        ],
    );
    write_lines(
        &claude.join("old.jsonl"),
        &[claude_turn(now - 5 * 24 * hour, "c", 5000, 0)],
    );
    let codex = repo
        .home
        .join(".codex/sessions/2026/10/01/rollout-2026-10-01T00-00-00-c1.jsonl");
    write_lines(&codex, &codex_rollout(now - hour, 93.0, 40.0));

    let usage = repo.json(&["usage", "--json"]);
    let logins = usage["logins"].as_array().unwrap();
    let claude = &logins[0];
    assert_eq!(claude["harness"], "claude-code");
    assert_eq!(claude["found"], true);
    // 100 in + 900 out (the later, fuller copy of msg-a) + 1000 in.
    assert_eq!(claude["five_hour"]["tokens"], 2000);
    assert_eq!(claude["weekly"]["tokens"], 7000);
    assert_eq!(claude["five_hour"]["used_percent"], Value::Null);
    let block_end = claude["five_hour"]["resets_at_ms"].as_u64().unwrap();
    assert!(
        block_end > now && block_end <= now + 3 * hour,
        "{block_end}"
    );
    assert!(claude["five_hour"]["cost_usd"].as_f64().unwrap() > 0.0);
    let codex = &logins[1];
    assert_eq!(codex["harness"], "codex");
    assert_eq!(codex["five_hour"]["used_percent"], 93.0);
    assert_eq!(codex["five_hour"]["source"], "rate_limits");
    assert_eq!(codex["weekly"]["used_percent"], 40.0);
    assert_eq!(codex["five_hour"]["tokens"], 1100);
    let table = repo.ok(&["usage"]);
    assert!(table.contains("codex        5-hour  93%"), "{table}");
    assert!(table.contains("claude_five_hour_tokens"), "{table}");

    // A budget gives Claude a percent.
    fs::write(
        repo.root.join("branchyard.toml"),
        "[usage]\nclaude_five_hour_tokens = 4000\n",
    )
    .unwrap();
    let usage = repo.json(&["usage", "--json"]);
    assert_eq!(usage["logins"][0]["five_hour"]["used_percent"], 50.0);
    assert_eq!(usage["logins"][0]["five_hour"]["source"], "budget");

    // The guard: warn by default, refuse when asked, nothing created.
    let agent = fake_agent!().display().to_string();
    let run = |name: &str| {
        repo.by(&[
            "run",
            "WRITE x.txt=1",
            "--name",
            name,
            "--harness",
            "codex-acp",
            "--command",
            &agent,
            "--yes",
        ])
    };
    let warned = run("warned");
    assert!(
        stderr(&warned).contains("the codex login has used 93% of its 5-hour window"),
        "{}",
        stderr(&warned)
    );
    fs::write(
        repo.root.join("branchyard.toml"),
        "[usage]\nguard = \"refuse\"\n",
    )
    .unwrap();
    let refused = run("refused");
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        stderr(&refused).contains("guard = \"refuse\""),
        "{}",
        stderr(&refused)
    );
    let names: Vec<String> = repo
        .json(&["ls", "--json"])
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["name"].as_str().unwrap().to_owned())
        .collect();
    assert!(!names.contains(&"refused".to_owned()), "{names:?}");
    fs::write(
        repo.root.join("branchyard.toml"),
        "[usage]\nguard = \"off\"\n",
    )
    .unwrap();
    assert!(!stderr(&run("quiet")).contains("login has used"));

    // A Claude limit message fills the window, with its reset.
    write_lines(
        &repo.home.join(".claude/projects/-w/limit.jsonl"),
        &[
            json!({"type": "assistant", "timestamp": rfc3339(now - hour), "isApiErrorMessage": true,
                 "message": {"model": "<synthetic>", "content": [{"type": "text",
                 "text": format!("Claude AI usage limit reached|{}", now / 1000 + 1800)}],
                 "usage": {"input_tokens": 0, "output_tokens": 0}}}),
        ],
    );
    let usage = repo.json(&["usage", "--json"]);
    let five = &usage["logins"][0]["five_hour"];
    assert_eq!(
        (five["limit_reached"].clone(), five["used_percent"].clone()),
        (json!(true), json!(100.0))
    );
    assert_eq!(five["resets_at_ms"], (now / 1000 + 1800) * 1000);
}

#[test]
fn the_router_skips_a_candidate_over_skip_over() {
    let repo = Repo::new("");
    let now = now_ms();
    write_lines(
        &repo
            .home
            .join(".codex/sessions/2026/10/01/rollout-2026-10-01T00-00-00-c1.jsonl"),
        &codex_rollout(now - 600_000, 99.0, 10.0),
    );
    let agent = fake_agent!().display().to_string();
    fs::write(
        repo.root.join("branchyard.toml"),
        format!(
            "[usage]\nskip_over = 95\n\n[fleet.default]\ncandidates = [\n  {{ harness = \"codex-acp\", command = \"{agent}\" }},\n  {{ harness = \"gemini-cli\", command = \"{agent}\" }},\n]\nexploration = 0.0\n"
        ),
    )
    .unwrap();
    let out = repo.by(&["run", "WRITE x.txt=1", "--auto", "--yes", "--seed", "1"]);
    let said = stderr(&out);
    assert!(out.status.success(), "{said}");
    assert!(said.contains("by:   gemini-cli"), "{said}");
    assert!(
        said.contains("not codex-acp: the codex login has used 99% of its 5-hour window"),
        "{said}"
    );
}

// Issue trackers.

/// A mock server answering each request with `answer` (status, extra
/// headers, JSON body); its URL, and the server itself, which records the
/// requests and fails the test if any connection errs.
fn serve(
    answer: impl Fn(&Request) -> (u16, Vec<(String, String)>, String) + Send + Sync + 'static,
) -> (String, MockHttp) {
    let mock = MockHttp::start(move |request| {
        let (status, headers, body) = answer(request);
        let mut response = Response::new(status, body).header("Content-Type", "application/json");
        for (name, value) in headers {
            response = response.header(&name, &value);
        }
        response
    });
    (mock.url(), mock)
}

/// The prompt and `issue_linked` event of `branch`.
fn linked(repo: &Repo, branch: &str) -> (String, Value) {
    let shown = repo.json(&["show", branch, "--json"]);
    let log = repo.json(&["log", branch, "--json"]);
    let link = log
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["pull_request"]["kind"] == "issue_linked")
        .map(|e| e["pull_request"].clone())
        .unwrap_or(Value::Null);
    (shown["prompt"].as_str().unwrap().to_owned(), link)
}

#[test]
fn issues_come_from_linear_jira_and_gitlab_and_tokens_stay_out_of_sight() {
    let (linear, linear_mock) = serve(|_| {
        (200, vec![], json!({"data": {"issue": {
            "identifier": "ENG-12", "title": "Parser panics", "description": "It **panics** on `\"\"`.",
            "url": "https://linear.app/acme/issue/ENG-12/parser-panics",
            "labels": {"nodes": [{"name": "bug"}]}}}}).to_string())
    });
    let (jira, jira_mock) = serve(|_| {
        (200, vec![], json!({"key": "PROJ-7", "fields": {"summary": "Login times out",
            "labels": ["auth"],
            "description": {"type": "doc", "version": 1, "content": [
                {"type": "paragraph", "content": [{"type": "text", "text": "Steps:"}]},
                {"type": "bulletList", "content": [{"type": "listItem", "content": [
                    {"type": "paragraph", "content": [{"type": "text", "text": "open /login"}]}]}]}]}}}).to_string())
    });
    let (gitlab, gitlab_mock) = serve(|seen| match seen.path.as_str() {
        "/api/v4/projects/acme%2Fwidgets/issues/12" => (
            200,
            vec![],
            json!({
                "iid": 12, "title": "Docs are stale", "description": "Update the README.",
                "web_url": "https://gitlab.example/acme/widgets/-/issues/12", "labels": ["docs"]})
            .to_string(),
        ),
        _ => (404, vec![], "{}".into()),
    });
    let mut repo = Repo::new(&format!("[trackers.jira]\nurl = \"{jira}\"\n"));
    repo.env
        .insert("LINEAR_API_URL".into(), format!("{linear}/graphql"));
    repo.env
        .insert("LINEAR_API_KEY".into(), "lin_api_SECRET1".into());
    repo.env
        .insert("JIRA_EMAIL".into(), "ada@example.com".into());
    repo.env
        .insert("JIRA_API_TOKEN".into(), "jira-SECRET2".into());
    repo.env.insert("GITLAB_URL".into(), gitlab.clone());
    repo.env
        .insert("GITLAB_TOKEN".into(), "glpat-SECRET3".into());

    let out = repo.agent(&["run", "--issue", "linear:ENG-12", "keep it small"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let (prompt, link) = linked(&repo, "eng-12-parser-panics");
    assert!(prompt.starts_with(
        "Resolve Linear issue ENG-12: Parser panics\nhttps://linear.app/acme/issue/ENG-12/parser-panics\nLabels: bug\n\nIt **panics**"
    ), "{prompt}");
    assert!(
        prompt.ends_with("Additional instructions:\nkeep it small\n"),
        "{prompt}"
    );
    assert_eq!(link["tracker"], "linear");
    assert_eq!(link["key"], "ENG-12");
    assert_eq!(link["number"], 12);
    let request = linear_mock.requests()[0].clone();
    assert_eq!(
        (request.method.as_str(), request.path.as_str()),
        ("POST", "/graphql")
    );
    assert_eq!(request.headers["authorization"], "lin_api_SECRET1");
    let body: Value = request.json();
    assert_eq!(body["variables"]["id"], "ENG-12");
    assert!(body["query"]
        .as_str()
        .unwrap()
        .contains("labels { nodes { name } }"));

    let out = repo.agent(&["run", "--issue", "jira:PROJ-7"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let (prompt, link) = linked(&repo, "proj-7-login-times-out");
    assert!(
        prompt.contains("Resolve Jira issue PROJ-7: Login times out\n"),
        "{prompt}"
    );
    assert!(
        prompt.contains(&format!("{jira}/browse/PROJ-7")),
        "{prompt}"
    );
    assert!(prompt.contains("Steps:\n\n- open /login"), "{prompt}");
    assert_eq!(link["tracker"], "jira");
    let request = jira_mock.requests()[0].clone();
    assert_eq!(
        request.path,
        "/rest/api/3/issue/PROJ-7?fields=summary,description,labels"
    );
    // Basic base64("ada@example.com:jira-SECRET2").
    assert_eq!(
        request.headers["authorization"],
        "Basic YWRhQGV4YW1wbGUuY29tOmppcmEtU0VDUkVUMg=="
    );

    let out = repo.agent(&["run", "--issue", "gitlab:acme/widgets#12", "--name", "gl"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let (prompt, link) = linked(&repo, "gl");
    assert!(
        prompt.starts_with("Resolve GitLab issue acme/widgets#12: Docs are stale\n"),
        "{prompt}"
    );
    assert_eq!(link["key"], "acme/widgets#12");
    assert_eq!(
        gitlab_mock.requests()[0].headers["private-token"],
        "glpat-SECRET3"
    );

    // A URL names the tracker too; a missing issue is refused by name.
    let out = repo.agent(&[
        "run",
        "--issue",
        &format!("{gitlab}/acme/widgets/-/issues/99"),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("GitLab has no issue acme/widgets#99 (404)"),
        "{}",
        stderr(&out)
    );

    // No token anywhere a person or a harness would see it.
    let mut seen = String::new();
    for branch in ["eng-12-parser-panics", "proj-7-login-times-out", "gl"] {
        seen.push_str(&repo.ok(&["log", branch, "--json"]));
        seen.push_str(&repo.ok(&["show", branch, "--json"]));
    }
    for secret in [
        "SECRET1",
        "SECRET2",
        "SECRET3",
        "YWRhQGV4YW1wbGUuY29tOmppcmEtU0VDUkVUMg",
    ] {
        assert!(!seen.contains(secret), "{secret} leaked");
    }

    // Without credentials: which variables it needs.
    repo.env.clear();
    let out = repo.agent(&["run", "--issue", "linear:ENG-12"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("needs LINEAR_API_KEY in the environment"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn an_issue_comes_through_the_connector_gateway_without_a_token() {
    let (gateway, mock) = serve(|seen| {
        let request: Value = serde_json::from_slice(&seen.body).unwrap_or_default();
        let id = request["id"].clone();
        let session = vec![("Mcp-Session-Id".to_owned(), "sess-1".to_owned())];
        match request["method"].as_str() {
            Some("initialize") => (200, session, json!({"jsonrpc": "2.0", "id": id, "result": {
                "protocolVersion": "2025-06-18", "capabilities": {}, "serverInfo": {"name": "mock"}}}).to_string()),
            Some("notifications/initialized") => (202, vec![], String::new()),
            Some("tools/call") => (200, vec![], json!({"jsonrpc": "2.0", "id": id, "result": {
                "content": [{"type": "text", "text": json!({"data": {"issue": {
                    "identifier": "ENG-5", "title": "From the gateway", "description": "Gated.",
                    "url": "https://linear.app/acme/issue/ENG-5"}}}).to_string()}]}}).to_string()),
            _ => (400, vec![], "{}".into()),
        }
    });
    let repo = Repo::new(&format!(
        "[connectors]\ngateway = \"{gateway}/mcp\"\n\n[trackers.linear]\ngateway_tool = \"linear__get_issue\"\n"
    ));
    let out = repo.agent(&["run", "--issue", "linear:ENG-5"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let (prompt, link) = linked(&repo, "eng-5-from-the-gateway");
    assert!(
        prompt.starts_with("Resolve Linear issue ENG-5: From the gateway\n"),
        "{prompt}"
    );
    assert_eq!(link["key"], "ENG-5");
    let seen = mock.requests();
    let methods: Vec<String> = seen
        .iter()
        .map(|s| {
            serde_json::from_slice::<Value>(&s.body).unwrap()["method"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(
        methods,
        ["initialize", "notifications/initialized", "tools/call"]
    );
    let call: Value = seen[2].json();
    assert_eq!(call["params"]["name"], "linear__get_issue");
    assert_eq!(call["params"]["arguments"]["id"], "ENG-5");
    assert_eq!(seen[2].headers["mcp-session-id"], "sess-1");
    // The token is the yard's, for this person, granting linear:read, and
    // verifies against the yard's public keys.
    let token = seen[2].headers["authorization"]
        .strip_prefix("Bearer ")
        .unwrap()
        .to_owned();
    let payload = token.split('.').nth(1).unwrap();
    let decoded = base64_url(payload);
    let claims: Value = serde_json::from_slice(&decoded).unwrap();
    assert_eq!(claims["aud"], format!("{gateway}/mcp"));
    assert_eq!(claims["by_grants"][0]["connector"], "linear");
    assert_eq!(claims["by_grants"][0]["mode"], "read");
    assert!(repo.root.join(".branchyard/gateway/jwks.json").is_file());
    let shown = repo.ok(&["log", "eng-5-from-the-gateway", "--json"]);
    assert!(!shown.contains(&token), "the token leaked into the log");
}

fn base64_url(text: &str) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text.trim_end_matches('='))
        .unwrap()
}

#[test]
fn a_branch_starts_from_a_pull_requests_head() {
    let repo = Repo::new("");
    // The pull request's head lives on the remote only, as refs/pull/7/head.
    let remote = repo.dir.join("remote.git");
    repo.git(&["init", "-q", "--bare", remote.to_str().unwrap()]);
    repo.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
    repo.git(&["checkout", "-q", "-b", "feature"]);
    fs::write(repo.root.join("a.txt"), "two\n").unwrap();
    repo.git(&["commit", "-q", "-am", "the change"]);
    let head = repo.git(&["rev-parse", "HEAD"]).trim().to_owned();
    repo.git(&["push", "-q", "origin", "HEAD:refs/pull/7/head"]);
    repo.git(&["checkout", "-q", "main"]);
    repo.git(&["branch", "-q", "-D", "feature"]);
    repo.git(&["reflog", "expire", "--expire=now", "--all"]);
    repo.git(&["gc", "-q", "--prune=now"]);
    let bin = repo.dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let gh = bin.join("gh");
    fs::write(
        &gh,
        format!(
            "#!/bin/sh\necho \"$*\" >> {log}\ncase \"$1 $2\" in\n\"auth status\") exit 0 ;;\n\"pr view\") cat <<'EOF'\n{json}\nEOF\n;;\n*) exit 3 ;;\nesac\n",
            log = repo.dir.join("gh.log").display(),
            json = json!({"number": 7, "title": "Faster parser", "body": "Halves the time.",
                "url": "https://github.com/acme/widgets/pull/7", "headRefName": "feature",
                "headRefOid": head, "baseRefName": "main", "isCrossRepository": false, "state": "OPEN"})
        ),
    )
    .unwrap();
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let agent = fake_agent!().display().to_string();
    let out = repo
        .command(env!("CARGO_BIN_EXE_by"))
        .env("PATH", &path)
        .args([
            "run",
            "--pr",
            "7",
            "add a benchmark",
            "--command",
            &agent,
            "--yes",
            "--harness",
            "gemini-cli",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let shown = repo.json(&["show", "pr-7-faster-parser", "--json"]);
    assert_eq!(shown["base"], head);
    let prompt = shown["prompt"].as_str().unwrap();
    assert!(prompt.starts_with("Continue GitHub pull request #7: Faster parser\nhttps://github.com/acme/widgets/pull/7\n"), "{prompt}");
    assert!(
        prompt.ends_with("Additional instructions:\nadd a benchmark\n"),
        "{prompt}"
    );
    assert_eq!(shown["merge_readiness"]["pull_request"]["number"], 7);
    assert_eq!(shown["merge_readiness"]["pull_request"]["head"], "feature");
    let worktree = shown["worktree"].as_str().unwrap();
    assert_eq!(
        fs::read_to_string(Path::new(worktree).join("a.txt")).unwrap(),
        "two\n"
    );
    let log = repo.ok(&["log", "pr-7-faster-parser"]);
    assert!(
        log.contains("started from pull request #7's head (feature)"),
        "{log}"
    );
    // --pr takes no --base, and no --issue.
    let out = repo.by(&["run", "--pr", "7", "--base", "main", "x"]);
    assert_eq!(out.status.code(), Some(2));
}

// Ports.

fn python() -> bool {
    Command::new("python3")
        .arg("-c")
        .arg("pass")
        .status()
        .is_ok_and(|s| s.success())
}

const SERVE: &str = "exec python3 -m http.server --bind 127.0.0.1 $BRANCHYARD_PORT";

#[test]
fn listening_ports_belong_to_their_branch_and_can_be_browsed_and_stopped() {
    if !python() {
        eprintln!("skipped: no python3 to listen with");
        return;
    }
    let repo = Repo::new(&format!(
        "[workspace]\n\n[workspace.run.web]\ncommand = \"{SERVE}\"\ndefault = true\n\n[workspace.run.api]\ncommand = \"{SERVE}\"\n"
    ));
    repo.ok(&["workspace", "trust"]);
    let out = repo.agent(&["run", "WRITE x.txt=1", "--name", "srv"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let quiet = repo.agent(&["run", "WRITE y.txt=1", "--name", "quiet"]);
    assert!(quiet.status.success());

    // Two scripts at once, each with its own port.
    let started = repo.json(&[
        "workspace",
        "run",
        "srv",
        "web",
        "api",
        "--detach",
        "--json",
    ]);
    let started = started["started"].as_array().unwrap().clone();
    assert_eq!(started.len(), 2);
    let ports: Vec<u64> = started
        .iter()
        .map(|s| s["port"].as_u64().unwrap())
        .collect();
    assert_ne!(ports[0], ports[1]);
    let reserved = repo.json(&["workspace", "show", "srv", "--json"])["port"]
        .as_u64()
        .unwrap();
    assert_eq!(ports[0], reserved);

    let listening = wait::until("both servers", || {
        let list = repo.json(&["workspace", "ports", "srv", "--json"]);
        (list.as_array().unwrap().len() == 2).then_some(list)
    });
    for l in listening.as_array().unwrap() {
        assert_eq!(l["branch"], "srv");
        assert_eq!(l["by"], "env", "{l}");
        assert!(ports.contains(&l["port"].as_u64().unwrap()));
    }
    // Only its own; the quiet branch has none.
    let all = repo.json(&["workspace", "ports", "--json"]);
    assert!(all.as_array().unwrap().iter().all(|l| l["branch"] == "srv"));
    assert!(repo
        .ok(&["workspace", "ports", "quiet"])
        .contains("listen on no TCP port"));
    // `by show` and `by workspace show` list them.
    let shown = repo.json(&["show", "srv", "--json"]);
    assert_eq!(shown["listening"].as_array().unwrap().len(), 2);
    assert!(repo
        .ok(&["show", "srv"])
        .contains(&format!(":{reserved} python3")));
    assert!(repo
        .ok(&["workspace", "show", "srv"])
        .contains("listening :"));

    // A server started by hand in the worktree, without Branchyard's
    // variables, is the branch's by its working directory.
    let worktree = shown["worktree"].as_str().unwrap().to_owned();
    let free = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut manual = Command::new("python3")
        .args([
            "-m",
            "http.server",
            "--bind",
            "127.0.0.1",
            &free.to_string(),
        ])
        .current_dir(&worktree)
        .env_remove("BRANCHYARD_BRANCH")
        .env_remove("BRANCHYARD_ROOT")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let by_cwd = wait::until("the hand-started server", || {
        repo.json(&["workspace", "ports", "srv", "--json"])
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["port"] == free)
            .cloned()
    });
    assert_eq!(by_cwd["by"], "cwd");

    // Browse: --print, and $BROWSER.
    assert_eq!(
        repo.ok(&[
            "workspace",
            "browse",
            "srv",
            "--port",
            &reserved.to_string(),
            "--print"
        ])
        .trim(),
        format!("http://127.0.0.1:{reserved}/")
    );
    let opened = repo.dir.join("opened.txt");
    let browser = repo.dir.join("browser.sh");
    fs::write(
        &browser,
        format!("#!/bin/sh\necho \"$1\" > {}\n", opened.display()),
    )
    .unwrap();
    fs::set_permissions(&browser, fs::Permissions::from_mode(0o755)).unwrap();
    let out = repo
        .command(env!("CARGO_BIN_EXE_by"))
        .env("BROWSER", &browser)
        .args([
            "workspace",
            "browse",
            "srv",
            "--port",
            &ports[1].to_string(),
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        fs::read_to_string(&opened).unwrap().trim(),
        format!("http://127.0.0.1:{}/", ports[1])
    );
    let out = repo.by(&["workspace", "browse", "quiet"]);
    assert!(
        stderr(&out).contains("listen on no TCP port"),
        "{}",
        stderr(&out)
    );

    // Kill: refused without a terminal or --yes, then every listener stops.
    let out = repo.by(&["workspace", "kill", "srv"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("pass --yes"), "{}", stderr(&out));
    let killed = repo.json(&["workspace", "kill", "srv", "--yes", "--json"]);
    assert_eq!(killed["stopped"].as_array().unwrap().len(), 3);
    let _ = manual.wait();
    wait::until("the servers to stop", || {
        repo.json(&["workspace", "ports", "srv", "--json"])
            .as_array()
            .unwrap()
            .is_empty()
            .then_some(())
    });
}

// Adopting sessions.

#[test]
fn adopt_lists_this_repositorys_sessions_and_resumes_one_natively() {
    let repo = Repo::new("");
    let root = repo.root.display().to_string();
    let encoded: String = root
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let now = now_ms();
    let claude_id = "0b5e7a1c-1111-4222-8333-944455556666";
    write_lines(
        &repo
            .home
            .join(format!(".claude/projects/{encoded}/{claude_id}.jsonl")),
        &[
            json!({"type": "user", "sessionId": claude_id, "cwd": root, "gitBranch": "main",
                   "timestamp": rfc3339(now - 600_000),
                   "message": {"role": "user", "content": "make the parser faster"}}),
            json!({"type": "assistant", "sessionId": claude_id, "cwd": root, "timestamp": rfc3339(now - 590_000),
                   "message": {"model": "claude-opus-4-8", "content": [{"type": "text", "text": "on it"}],
                               "usage": {"input_tokens": 1, "output_tokens": 1}}}),
        ],
    );
    // Another repository's session, and a Codex subagent's: neither listed.
    write_lines(
        &repo.home.join(".claude/projects/-elsewhere/x.jsonl"),
        &[
            json!({"type": "user", "cwd": "/elsewhere", "timestamp": rfc3339(now),
                 "message": {"content": "not ours"}}),
        ],
    );
    let commit = repo.git(&["rev-parse", "HEAD"]).trim().to_owned();
    let codex_id = "019a0000-aaaa-7bbb-8ccc-dddddddddddd";
    let rollout = |id: &str, source: Value, text: &str| {
        vec![
            json!({"timestamp": rfc3339(now - 3_600_000), "type": "session_meta",
                   "payload": {"id": id, "cwd": root, "source": source,
                               "git": {"commit_hash": commit, "branch": "main"}}}),
            json!({"timestamp": rfc3339(now - 3_590_000), "type": "event_msg",
                   "payload": {"type": "user_message", "message": text}}),
        ]
    };
    write_lines(
        &repo.home.join(format!(
            ".codex/sessions/2026/10/01/rollout-2026-10-01T00-00-00-{codex_id}.jsonl"
        )),
        &rollout(codex_id, json!("cli"), "write the docs"),
    );
    write_lines(
        &repo
            .home
            .join(".codex/sessions/2026/10/01/rollout-sub.jsonl"),
        &rollout(
            "019a-sub",
            json!({"subagent": {"thread_spawn": {}}}),
            "review pass",
        ),
    );

    let listed = repo.json(&["adopt", "--json"]);
    let ids: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [claude_id, codex_id]);
    assert_eq!(listed[0]["title"], "make the parser faster");
    assert_eq!(listed[1]["commit"], commit);
    let text = repo.ok(&["adopt"]);
    assert!(text.contains("0b5e7a1c  claude-code"), "{text}");

    // The checkout has uncommitted work: it comes along, and says so.
    fs::write(repo.root.join("a.txt"), "edited by the session\n").unwrap();
    fs::write(repo.root.join("new.txt"), "untracked\n").unwrap();
    let adopted = repo.json(&[
        "adopt",
        "0b5e7a1c",
        "--name",
        "faster",
        "--harness",
        "claude-code-acp",
        "--json",
    ]);
    assert_eq!(adopted["branch"], "faster");
    assert_eq!(adopted["how"], "head_with_diff");
    assert_eq!(adopted["diff_files"], json!(["a.txt"]));
    let notes = adopted["notes"].to_string();
    assert!(notes.contains("untracked file"), "{notes}");
    assert!(
        notes.contains("not necessarily all the session's"),
        "{notes}"
    );
    let worktree = PathBuf::from(adopted["worktree"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(worktree.join("a.txt")).unwrap(),
        "edited by the session\n"
    );
    assert!(!worktree.join("new.txt").exists());
    // Claude Code finds the transcript where it looks for the worktree's.
    let placed = PathBuf::from(adopted["transcript_placed"].as_str().unwrap());
    let worktree_encoded: String = fs::canonicalize(&worktree)
        .unwrap()
        .display()
        .to_string()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    assert_eq!(
        placed,
        repo.home.join(format!(
            ".claude/projects/{worktree_encoded}/{claude_id}.jsonl"
        ))
    );
    assert!(placed.is_file());
    let shown = repo.json(&["show", "faster", "--json"]);
    assert_eq!(shown["session"], claude_id);
    assert_eq!(shown["status"]["state"], "no_changes");
    assert_eq!(shown["profile"], "claude-code-acp");
    let log = repo.json(&["log", "faster", "--json"]);
    let event = log
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["activity"] == "adopted")
        .unwrap();
    assert_eq!(event["adopted"]["session"], claude_id);
    assert_eq!(event["adopted"]["harness"], "claude-code");

    // Its next turn resumes the session natively.
    let agent = fake_agent!().display().to_string();
    let out = repo.by(&["send", "faster", "WHOAMI", "--command", &agent, "--yes"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains(&format!("session {claude_id} resumed=true")),
        "{}",
        stdout(&out)
    );

    // A Codex session starts at the commit it recorded; --no-diff keeps
    // the checkout's edits out.
    let adopted = repo.json(&["adopt", codex_id, "--no-diff", "--json"]);
    assert_eq!(adopted["how"], "session_commit");
    assert_eq!(adopted["base"], commit);
    assert!(adopted["notes"]
        .to_string()
        .contains("--no-diff left them out"));
    let worktree = PathBuf::from(adopted["worktree"].as_str().unwrap());
    assert_eq!(fs::read_to_string(worktree.join("a.txt")).unwrap(), "one\n");
    assert_eq!(adopted["transcript_placed"], Value::Null);

    // Unknown and wrong-harness requests are refused.
    assert!(stderr(&repo.by(&["adopt", "nope"])).contains("no Claude Code or Codex session"));
    let out = repo.by(&["adopt", codex_id, "--harness", "claude-code-acp"]);
    assert!(
        stderr(&out).contains("is not a profile of codex"),
        "{}",
        stderr(&out)
    );
}
