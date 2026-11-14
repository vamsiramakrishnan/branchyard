//! `[models]`, `--model-gateway`, `by models`, and what `by show`, `by log`
//! and `by stats` say about model calls, end to end with the built `by`
//! and the fake ACP agent. The upstream is a mock of Anthropic's API on
//! this machine's loopback; the harness is a Python script calling it
//! through the gateway by `ANTHROPIC_BASE_URL` and `ANTHROPIC_API_KEY`.
//! Requires `git`, `sh` and `python3`.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
use std::fs;
use std::path::PathBuf;
use std::process::Output;

use branchyard_testkit::fake_agent;
use branchyard_testkit::{MockHttp, Response};
use serde_json::json;

const KEY: &str = "sk-real-anthropic-cli";

const HARNESS: &str = r#"import json, os, sys, urllib.error, urllib.request
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
print("real keys in env:", ",".join(sorted(k for k, v in os.environ.items() if "sk-real" in v)) or "none")
for model in sys.argv[1:]:
    body = {"model": model, "max_tokens": 16, "messages": [{"role": "user", "content": "hi"}]}
    request = urllib.request.Request(
        os.environ["ANTHROPIC_BASE_URL"] + "/v1/messages", data=json.dumps(body).encode(),
        headers={"x-api-key": os.environ["ANTHROPIC_API_KEY"], "anthropic-version": "2023-06-01",
                 "content-type": "application/json"}, method="POST")
    try:
        with opener.open(request, timeout=60) as response:
            # Read the answer as a harness does: closing before the gateway
            # has written it is a harness that went away.
            response.read()
            print(f"call {model}: {response.status}")
    except urllib.error.HTTPError as error:
        # Read the error to its end, as an SDK does: a test about a harness
        # that goes away says so.
        error.read()
        print(f"call {model}: {error.code}")
"#;

/// A mock of Anthropic's Messages API: usage of 1000 input and 100 output
/// tokens per call, and an answer that calls the tools Read and Edit. The `x-api-key` of each request it has answered is
/// [`keys`].
fn upstream() -> MockHttp {
    MockHttp::start(|request| {
        let model = request.json()["model"].as_str().unwrap_or("").to_owned();
        Response::json(
            200,
            &json!({"type": "message", "model": model,
                "content": [{"type": "text", "text": "hi"},
                    {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {}},
                    {"type": "tool_use", "id": "toolu_2", "name": "Edit", "input": {}}],
                "stop_reason": "tool_use",
                "usage": {"input_tokens": 1000, "output_tokens": 100}}),
        )
    })
}

/// The `x-api-key` of each request `upstream` has answered, oldest first.
fn keys(upstream: &MockHttp) -> Vec<String> {
    upstream
        .requests()
        .iter()
        .map(|r| r.header("x-api-key").unwrap_or_default().to_owned())
        .collect()
}

/// The kit's repository, plus the harness script beside it.
struct Repo {
    kit: branchyard_testkit::Repo,
    harness: PathBuf,
}

impl std::ops::Deref for Repo {
    type Target = branchyard_testkit::Repo;
    fn deref(&self) -> &Self::Target {
        &self.kit
    }
}

impl Repo {
    fn new(port: u16) -> Repo {
        Repo::with(port, "")
    }

    /// With `extra` (lines of TOML) in `[models]`.
    fn with(port: u16, extra: &str) -> Repo {
        let kit = branchyard_testkit::repo!();
        let repo = Repo {
            harness: kit.dir.join("harness.py"),
            kit,
        };
        fs::write(&repo.harness, HARNESS).unwrap();
        fs::write(repo.dir.join("anthropic-key"), KEY).unwrap();
        fs::write(
            repo.root.join("branchyard.toml"),
            format!(
                "version = 1\n\n[secrets]\nanthropic = \"@{}\"\n\n[models]\nallow = [\"claude-*\"]\n{extra}\n\
                 [models.backends.anthropic]\napi = \"anthropic\"\nurl = \"http://127.0.0.1:{port}\"\n\
                 key = \"anthropic\"\n\n[[models.routes]]\nmodel = \"claude-*\"\n\
                 backends = [\"anthropic\"]\nrequests_per_minute = 100\n\n\
                 [models.budget]\nmonthly_usd = 10.0\n",
                repo.dir.join("anthropic-key").display()
            ),
        )
        .unwrap();
        repo.git(&["add", "."]);
        repo.git(&["commit", "-q", "--amend", "--no-edit"]);
        repo
    }

    fn run(&self, name: &str, flags: &[&str], models: &[&str]) -> Output {
        let agent = fake_agent!().display().to_string();
        let prompt = format!("SH python3 {} {}", self.harness.display(), models.join(" "));
        let mut args = vec![
            "run",
            "--name",
            name,
            "--harness",
            "gemini-cli",
            "--command",
            &agent,
            "--yes",
        ];
        args.extend(flags);
        args.extend(["--", &prompt]);
        self.by(&args)
    }

    fn text(&self, args: &[&str]) -> String {
        let out = self.by(args);
        assert!(out.status.success(), "{args:?}: {}", stderr(&out));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn branches_use_the_gateway_by_configuration_and_every_surface_shows_it() {
    let upstream = upstream();
    let repo = Repo::new(upstream.port());
    // `[models] allow` puts a new branch on the gateway.
    let out = repo.run("a", &[], &["claude-sonnet-4-6"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(keys(&upstream), [KEY]);
    // Sonnet 4.6 at $3 in and $15 out per million.
    let cost = (1000.0 * 3.0 + 100.0 * 15.0) / 1e6;
    let shown = repo.json(&["show", "a", "--json"]);
    assert_eq!(shown["models"]["mode"], "gateway");
    assert_eq!(shown["models"]["models"], json!(["claude-*"]));
    assert_eq!(shown["models"]["calls"], 1);
    assert_eq!(shown["models"]["input_tokens"], 1000);
    assert_eq!(shown["models"]["output_tokens"], 100);
    // The answer called Read and Edit.
    assert_eq!(shown["models"]["tool_uses"], 2);
    let metered = shown["models"]["cost_usd"].as_f64().unwrap();
    assert!((metered - cost).abs() < 1e-12, "{metered}");
    assert_eq!(shown["cost_usd"].as_f64(), Some(metered));
    let text = repo.text(&["show", "a"]);
    assert!(
        text.contains("gateway (claude-*); 1 call, 1.0k in / 100 out tokens, $0.0045"),
        "{text}"
    );
    let log = repo.text(&["log", "a"]);
    assert!(log.contains("real keys in env: none"), "{log}");
    assert!(
        log.contains(
            "model: claude-sonnet-4-6 allowed via anthropic (200, 1000 in / 100 out, $0.0045"
        ),
        "{log}"
    );
    assert!(
        log.contains("$0.0045, tools: Read, Edit, stop: tool_use, "),
        "{log}"
    );
    assert!(!log.contains(KEY));
    let log = repo.text(&["log", "a", "--json"]);
    assert!(log.contains(r#""activity": "model""#), "{log}");
    assert!(log.contains(r#""prompt_digest""#), "{log}");
    assert!(log.contains(r#""stop_reason": "tool_use""#), "{log}");
    assert!(!log.contains("audit_file"), "{log}");
    // A flag narrows what the configuration allows, for that branch.
    let out = repo.run(
        "b",
        &["--model-gateway=claude-haiku-*"],
        &["claude-sonnet-4-6"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let shown = repo.json(&["show", "b", "--json"]);
    assert_eq!(shown["models"]["models"], json!(["claude-haiku-*"]));
    assert_eq!(shown["models"]["calls"], 0);
    assert_eq!(shown["models"]["refused"], json!({"denied": 1}));
    assert_eq!(keys(&upstream), [KEY], "a refused call was forwarded");
    // `by models`: its backends, routes, budget and usage.
    let models = repo.json(&["models", "--json"]);
    assert_eq!(models["configured"], true);
    assert_eq!(models["backends"][0]["name"], "anthropic");
    assert_eq!(models["backends"][0]["key_set"], true);
    assert!(models["backends"][0]["key"]
        .as_str()
        .unwrap()
        .ends_with("anthropic-key"));
    assert_eq!(models["routes"][0]["requests_per_minute"], 100);
    assert_eq!(models["audit"], "digests");
    assert_eq!(models["usage"]["calls"], 1);
    assert_eq!(models["by_model"]["claude-sonnet-4-6"]["calls"], 1);
    assert_eq!(models["budget"]["monthly_usd"], 10.0);
    assert_eq!(models["budget"]["this_month"]["calls"], 1);
    let text = repo.text(&["models"]);
    for line in [
        "backends",
        "anthropic      anthropic",
        "routes",
        "claude-*       anthropic; 100 requests a minute",
        "monthly $0.0045 of $10.00 (0%)",
        "usage this month",
        "1 call, 1.0k in / 100 out tokens, $0.0045",
        "audit",
        "digests: sizes, a request digest, tools and stop reason per call",
    ] {
        assert!(text.contains(line), "{line:?} not in {text}");
    }
    assert!(!text.contains(KEY));
    // `by stats`: the calls by decision and their metered cost.
    let stats = repo.json(&["stats", "--json"]);
    assert_eq!(stats["model_calls"], json!({"allowed": 1, "denied": 1}));
    assert!((stats["model_cost_usd"].as_f64().unwrap() - cost).abs() < 1e-12);
    // A bad [models] is refused by name.
    fs::write(
        repo.root.join("branchyard.toml"),
        "version = 1\n\n[[models.routes]]\nmodel = \"*\"\nbackends = [\"nowhere\"]\n",
    )
    .unwrap();
    let out = repo.by(&["config", "validate"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("no backend nowhere")
            || String::from_utf8_lossy(&out.stdout).contains("no backend nowhere")
    );
}

#[test]
fn contents_are_kept_when_the_configuration_says_so_and_another_mode_is_refused_by_name() {
    let upstream = upstream();
    let repo = Repo::with(upstream.port(), "audit = \"contents\"\n");
    let out = repo.run("c", &[], &["claude-sonnet-4-6"]);
    assert!(out.status.success(), "{}", stderr(&out));
    // The branch's events name the file, not the text.
    let log = repo.text(&["log", "c", "--json"]);
    assert!(
        log.contains(r#""audit_file": ".branchyard/models/audit/c/1.jsonl""#),
        "{log}"
    );
    assert!(!log.contains(r#""content""#), "{log}");
    // The file holds the request and the response.
    let text = fs::read_to_string(repo.root.join(".branchyard/models/audit/c/1.jsonl")).unwrap();
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 1, "{text}");
    assert_eq!(lines[0]["model"], "claude-sonnet-4-6");
    assert_eq!(lines[0]["backend"], "anthropic");
    assert_eq!(lines[0]["request"]["messages"][0]["content"], "hi");
    assert_eq!(lines[0]["response"]["content"][1]["name"], "Read");
    assert!(!text.contains(KEY));
    let models = repo.json(&["models", "--json"]);
    assert_eq!(models["audit"], "contents");
    let text = repo.text(&["models"]);
    assert!(
        text.contains("contents: also each call's request and response"),
        "{text}"
    );
    // Any other value is refused, naming it and the two there are.
    fs::write(
        repo.root.join("branchyard.toml"),
        "version = 1\n\n[models]\naudit = \"full\"\n",
    )
    .unwrap();
    let out = repo.by(&["config", "validate"]);
    assert!(!out.status.success());
    let said = format!("{}{}", stderr(&out), String::from_utf8_lossy(&out.stdout));
    for needle in ["`full`", "`digests`", "`contents`"] {
        assert!(said.contains(needle), "{needle} not in {said}");
    }
}

const AIPLEX_TOKEN: &str = "aiplex-real-token-cli";

/// An AIPlex access point: a model with `refuse-429` in its name is refused
/// by AIPlex itself; the rest answered as by the provider. Every answer
/// carries AIPlex's request id, `req-N` for the N-th.
fn access_point() -> MockHttp {
    let n = std::sync::atomic::AtomicUsize::new(0);
    MockHttp::start(move |request| {
        let id = format!(
            "req-{}",
            n.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
        );
        let model = request.json()["model"].as_str().unwrap_or("").to_owned();
        if model.contains("refuse-429") {
            return Response::json(
                429,
                &json!({"type": "error", "error": {"type": "rate_limit_error",
                    "message": "QUOTA_BUDGET_EXHAUSTED: the budget is spent"}}),
            )
            .header("X-Aiplex-Request-Id", &id)
            .header("X-Aiplex-Error-Code", "QUOTA_BUDGET_EXHAUSTED")
            .header("Retry-After", "30");
        }
        Response::json(
            200,
            &json!({"type": "message", "model": model, "content": [{"type": "text", "text": "hi"}],
                "usage": {"input_tokens": 1000, "output_tokens": 100}}),
        )
        .header("X-Aiplex-Request-Id", &id)
    })
}

#[test]
fn an_aiplex_backend_is_shown_its_refusal_is_final_and_bad_ones_are_refused_by_name() {
    let access = access_point();
    let repo = Repo::new(upstream().port());
    let token = repo.dir.join("aiplex-token");
    fs::write(&token, AIPLEX_TOKEN).unwrap();
    let backend = format!(
        "[models.backends.aiplex-anthropic]\napi = \"anthropic\"\nurl = \"{}/ap/ide\"\n\
         key = \"aiplex\"\ncontrol_plane = \"aiplex\"\n\n",
        access.url()
    );
    fs::write(
        repo.root.join("branchyard.toml"),
        format!(
            "version = 1\n\n[secrets]\naiplex = \"@{}\"\n\n[models]\nallow = [\"claude-*\"]\n\
             audit = \"contents\"\n\n{backend}[[models.routes]]\nmodel = \"claude-*\"\n\
             backends = [\"aiplex-anthropic\"]\n",
            token.display()
        ),
    )
    .unwrap();
    repo.git(&["add", "."]);
    repo.git(&["commit", "-q", "--amend", "--no-edit"]);
    let out = repo.run("cp", &[], &["claude-sonnet-4-6", "claude-refuse-429"]);
    assert!(out.status.success(), "{}", stderr(&out));
    // AIPlex was called once per call, with its token as a bearer token.
    let seen = access.requests();
    assert_eq!(seen.len(), 2);
    for request in &seen {
        let bearer = format!("Bearer {AIPLEX_TOKEN}");
        assert_eq!(request.header_values("authorization"), [bearer.as_str()]);
        assert!(request.header_values("x-api-key").is_empty());
        assert_eq!(
            request.header("X-AIPlex-Tags"),
            Some("by_branch=cp,by_turn=1,by_harness=gemini-cli")
        );
    }
    let log = repo.text(&["log", "cp"]);
    assert!(log.contains("call claude-refuse-429: 429"), "{log}");
    assert!(
        log.contains(
            "model: claude-sonnet-4-6 allowed via aiplex-anthropic (200, 1000 in / 100 out"
        ),
        "{log}"
    );
    // AIPlex's id, then the call's own (AIPlex's `client_request_id`).
    assert!(log.contains("aiplex request req-1, call "), "{log}");
    assert!(
        log.contains(
            "model: claude-refuse-429 refused via aiplex-anthropic \
             (429, refused by aiplex (QUOTA_BUDGET_EXHAUSTED), "
        ),
        "{log}"
    );
    assert!(log.contains("aiplex request req-2, call "), "{log}");
    let json_log = repo.text(&["log", "cp", "--json"]);
    assert!(json_log.contains(r#""request_id": "req-2""#), "{json_log}");
    assert!(
        json_log.contains(r#""error_code": "QUOTA_BUDGET_EXHAUSTED""#),
        "{json_log}"
    );
    assert!(json_log.contains(r#""plane": "aiplex""#), "{json_log}");
    assert!(json_log.contains(r#""call_id": ""#), "{json_log}");
    let shown = repo.json(&["show", "cp", "--json"]);
    assert_eq!(shown["models"]["refused"], json!({"refused": 1}));
    let models = repo.json(&["models", "--json"]);
    assert_eq!(models["backends"][0]["control_plane"], "aiplex");
    let text = repo.text(&["models"]);
    assert!(text.contains("control plane aiplex"), "{text}");
    let audit = fs::read_to_string(repo.root.join(".branchyard/models/audit/cp/1.jsonl")).unwrap();
    for said in [&log, &json_log, &text, &audit] {
        assert!(!said.contains(AIPLEX_TOKEN), "the token was shown: {said}");
    }
    // Refused when read, by name.
    let plain = "[models.backends.direct]\napi = \"anthropic\"\nkey = \"anthropic\"\n\n";
    let address = format!("url = \"{}/ap/ide\"\n", access.url());
    assert!(backend.contains(&address), "{backend}");
    for (toml, needles) in [
        (
            format!(
                "{backend}{plain}[[models.routes]]\nmodel = \"claude-*\"\n\
                 backends = [\"aiplex-anthropic\"]\nfallbacks = [\"direct\"]\n"
            ),
            vec!["models.routes[0]", "aiplex-anthropic", "direct"],
        ),
        (
            format!("{backend}{plain}"),
            vec![
                "models.backends.aiplex-anthropic",
                "add a route that names it",
            ],
        ),
        (
            backend.replace("control_plane = \"aiplex\"", "control_plane = \"aiplexx\""),
            vec![
                "models.backends.aiplex-anthropic.control_plane",
                "\"aiplexx\"",
                "\"aiplex\"",
            ],
        ),
        // Without a url its calls, and the token, would go to the
        // provider's public API: with a stored token and with a signed one.
        (
            backend.replace(&address, ""),
            vec![
                "models.backends.aiplex-anthropic.url",
                "has no default address",
                "url = \"https://<aiplex>/ap/<access point>\"",
            ],
        ),
        (
            backend.replace(&address, "").replace(
                "key = \"aiplex\"",
                "auth = \"turn-token\"\naudience = \"https://aiplex.example\"",
            ),
            vec![
                "models.backends.aiplex-anthropic.url",
                "has no default address",
            ],
        ),
    ] {
        fs::write(
            repo.root.join("branchyard.toml"),
            format!("version = 1\n\n{toml}"),
        )
        .unwrap();
        let out = repo.by(&["config", "validate"]);
        assert!(!out.status.success(), "{toml}");
        let said = format!("{}{}", stderr(&out), String::from_utf8_lossy(&out.stdout));
        for needle in needles {
            assert!(said.contains(needle), "{needle:?} not in {said}");
        }
    }
    access.finish();
}
