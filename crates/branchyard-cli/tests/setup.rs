//! `by init` and `by config` end to end: the built binary in temporary
//! repositories, driven the way a harness drives the protocol. Hermetic:
//! the user configuration is redirected, no model is called, and every
//! generated secret is checked to appear in no output.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Map, Value};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Repo {
    root: PathBuf,
    /// Every byte `by` printed, to check no secret ever appears.
    printed: std::cell::RefCell<String>,
}

impl Repo {
    fn new() -> Repo {
        let dir = std::env::temp_dir().join(format!(
            "branchyard-setup-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        if dir.exists() {
            fs::remove_dir_all(&dir).unwrap();
        }
        fs::create_dir_all(dir.join("app")).unwrap();
        let root = fs::canonicalize(dir.join("app")).unwrap();
        branchyard_testkit::system_tools(&root.parent().unwrap().join("tools")).unwrap();
        let repo = Repo {
            root,
            printed: Default::default(),
        };
        let git = Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&repo.root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .unwrap();
        assert!(git.success());
        fs::write(repo.root.join("Cargo.toml"), "[package]\nname = \"app\"\n").unwrap();
        repo
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_by"));
        branchyard_testkit::hermetic(&mut command)
            .current_dir(&self.root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("NO_COLOR", "1")
            .env("PATH", self.root.parent().unwrap().join("tools"))
            .env("BRANCHYARD_HARNESS_DIRS", "")
            .env("HOME", self.root.parent().unwrap().join("home"))
            .env(
                "BRANCHYARD_USER_CONFIG",
                self.root.parent().unwrap().join("user.toml"),
            )
            .stdin(Stdio::null());
        command.env_remove("XDG_CONFIG_HOME");
        // The credential variables setup reports, by name: none of this
        // machine's, so the questions they add are the test's choice.
        for name in branchyard_setup::probe::KNOWN_VARIABLES {
            command.env_remove(name);
        }
        command
    }

    fn record(&self, out: &Output) {
        let mut printed = self.printed.borrow_mut();
        printed.push_str(&String::from_utf8_lossy(&out.stdout));
        printed.push_str(&String::from_utf8_lossy(&out.stderr));
    }

    fn by(&self, args: &[&str]) -> Output {
        let out = self.command().args(args).output().unwrap();
        self.record(&out);
        out
    }

    fn by_stdin(&self, args: &[&str], input: &str) -> Output {
        let mut child = self
            .command()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        self.record(&out);
        out
    }

    fn json(&self, args: &[&str]) -> Value {
        let out = self.by(args);
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!("by {args:?}: {e}\n{}", String::from_utf8_lossy(&out.stderr))
        })
    }

    fn write_answers(&self, answers: &Map<String, Value>) -> String {
        let path = self.root.parent().unwrap().join("answers.json");
        fs::write(&path, serde_json::to_string(answers).unwrap()).unwrap();
        path.display().to_string()
    }

    /// Every secret file under the repository: none may appear in output.
    fn assert_no_secret_printed(&self) {
        let printed = self.printed.borrow();
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                if path.is_dir() {
                    if name != ".git" {
                        stack.push(path);
                    }
                } else if name.ends_with(".token")
                    || name.ends_with(".secret")
                    || name == "db-password.txt"
                {
                    let secret = fs::read_to_string(&path).unwrap();
                    let secret = secret.trim();
                    assert!(secret.len() >= 16, "{}", path.display());
                    assert!(!printed.contains(secret), "{} was printed", path.display());
                    assert_eq!(mode(&path), 0o600, "{}", path.display());
                }
            }
        }
    }
}

fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// A scripted harness: ask `--next`, answer every question with its first
/// choice's label (what clicking the recommended option in
/// `AskUserQuestion` returns), keep the normalized answers, repeat.
fn walk(repo: &Repo, topic: &str) -> (Map<String, Value>, Value) {
    let mut answers = Map::new();
    for round in 0..20 {
        let file = repo.write_answers(&answers);
        let response = repo.json(&["init", topic, "--json", "--next", "--answers", &file]);
        assert_eq!(response["protocol"], json!("branchyard.setup/v1"));
        assert_eq!(response["errors"], json!([]), "{topic} round {round}");
        if response["done"] == json!(true) {
            return (answers, response);
        }
        let questions = response["questions"].as_array().unwrap();
        assert!(
            !questions.is_empty() && questions.len() <= 4,
            "{topic}: {}",
            questions.len()
        );
        for (id, value) in response["answers"].as_object().unwrap() {
            answers.insert(id.clone(), value.clone());
        }
        for q in questions {
            let choices = q["choices"].as_array().unwrap();
            assert!(!choices.is_empty() && choices.len() <= 4);
            assert!(q["header"].as_str().unwrap().chars().count() <= 12);
            answers.insert(
                q["id"].as_str().unwrap().into(),
                choices[0]["label"].clone(),
            );
        }
    }
    panic!("{topic} did not finish");
}

#[test]
fn a_scripted_harness_completes_every_topic_through_the_protocol() {
    for topic in ["project", "server", "rig", "deploy", "plugin"] {
        let repo = Repo::new();
        let (answers, done) = walk(&repo, topic);
        let plan = &done["plan"];
        assert_eq!(plan["valid"], json!(true), "{topic}: {plan:#}");
        let file = repo.write_answers(&answers);

        let dry = repo.json(&["init", topic, "--answers", &file, "--dry-run", "--json"]);
        assert_eq!(dry["plan"]["valid"], json!(true));
        for f in dry["plan"]["files"].as_array().unwrap() {
            let path = repo.root.join(f["path"].as_str().unwrap());
            assert!(
                !path.exists() || f["path"].as_str().unwrap().starts_with('/'),
                "a dry run wrote {path:?}"
            );
        }

        let applied = repo.json(&["init", topic, "--answers", &file, "--apply", "--json"]);
        let written = applied["written"].as_array().unwrap();
        for path in written {
            let path = path.as_str().unwrap();
            assert!(
                path.starts_with('/') || repo.root.join(path).exists(),
                "{topic}: {path}"
            );
        }
        // Applying again changes nothing.
        let again = repo.json(&["init", topic, "--answers", &file, "--apply", "--json"]);
        assert_eq!(again["written"], json!([]), "{topic}: {again:#}");
        repo.assert_no_secret_printed();
    }
}

#[test]
fn every_generated_file_passes_the_tool_that_reads_it() {
    let repo = Repo::new();
    let out = repo.by(&["init", "server", "--defaults", "--apply"]);
    assert!(out.status.success(), "{}", text(&out));
    let check = repo.by(&["serve", "--config", ".branchyard/server.json", "--check"]);
    assert!(check.status.success(), "{}", text(&check));
    assert!(text(&check).contains("configuration ok"));

    let out = repo.by(&["init", "rig", "--defaults", "--apply"]);
    assert!(out.status.success(), "{}", text(&out));
    let check = repo.by(&["rig", "check", "rigs/team.toml"]);
    assert!(check.status.success(), "{}", text(&check));

    let out = repo.by(&["init", "project", "--defaults", "--apply"]);
    assert!(out.status.success(), "{}", text(&out));
    let check = repo.by(&["config", "validate"]);
    assert!(check.status.success(), "{}", text(&check));
    let schema_hint = fs::read_to_string(repo.root.join("branchyard.toml")).unwrap();
    assert!(schema_hint.starts_with("#:schema https://"));
    repo.assert_no_secret_printed();
}

/// `by init project` reads the worktree configuration another tool
/// committed (each fixture written for this test) into its `[workspace]`
/// suggestion, and the file it writes loads.
#[test]
fn project_setup_imports_another_tools_workspace_configuration() {
    for (file, content, setup, run, teardown) in [
        (
            ".emdash.json",
            r#"{"preservePatterns": [".env"], "scripts": {"setup": "pnpm install",
                "run": "PORT=$EMDASH_PORT pnpm dev", "teardown": "docker compose down"}}"#,
            "pnpm install",
            "PORT=$BRANCHYARD_PORT pnpm dev",
            "docker compose down",
        ),
        (
            "orca.yaml",
            "scripts:\n  setup: |\n    pnpm install\n  archive: docker compose down\n\
             defaultTabs:\n  - title: Dev\n    command: pnpm dev\n",
            "pnpm install",
            "pnpm dev",
            "docker compose down",
        ),
        (
            ".superset/config.json",
            r#"{"setup": ["bun install"], "run": ["bun dev"], "teardown": ["docker compose down"]}"#,
            "bun install",
            "bun dev",
            "docker compose down",
        ),
        (
            ".conductor/settings.toml",
            "[scripts]\nsetup = \"uv sync\"\narchive = \"docker compose down\"\n\
             [scripts.run.web]\ncommand = \"uv run app --port $CONDUCTOR_PORT\"\n",
            "uv sync",
            "uv run app --port $BRANCHYARD_PORT",
            "docker compose down",
        ),
    ] {
        let repo = Repo::new();
        let path = repo.root.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
        let out = repo.by(&["init", "project", "--defaults", "--apply"]);
        assert!(out.status.success(), "{file}: {}", text(&out));
        let written = fs::read_to_string(repo.root.join("branchyard.toml")).unwrap();
        let config: toml_edit::DocumentMut = written.parse().unwrap();
        let workspace = &config["workspace"];
        assert_eq!(
            workspace["setup"].as_str(),
            Some(setup),
            "{file}: {written}"
        );
        assert_eq!(
            workspace["teardown"].as_str(),
            Some(teardown),
            "{file}: {written}"
        );
        let runs = workspace["run"].as_table_like().unwrap();
        let (_, script) = runs.iter().next().unwrap();
        assert_eq!(script["command"].as_str(), Some(run), "{file}: {written}");
        let check = repo.by(&["config", "validate"]);
        assert!(check.status.success(), "{file}: {}", text(&check));
    }
}

#[test]
fn a_multi_tenant_server_gets_one_0600_token_per_tenant_and_its_hash_only() {
    let repo = Repo::new();
    let answers = json!({
        "tenancy": "Several teams", "tenants": "acme, globex", "quota.max_running": 3,
        "listen": "0.0.0.0:8421", "tls": "no", "webhook.url": "https://hooks.example.com/by"
    });
    let out = repo.by_stdin(
        &[
            "init",
            "server",
            "--answers",
            "-",
            "--defaults",
            "--dry-run",
            "--json",
        ],
        &answers.to_string(),
    );
    let dry: Value = serde_json::from_slice(&out.stdout).unwrap();
    // Off loopback without TLS: the server's own loader refuses it.
    assert_eq!(dry["plan"]["valid"], json!(false), "{dry:#}");
    let config = dry["plan"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["kind"] == json!("server_config"))
        .unwrap();
    assert!(config["validation"]["messages"][0]
        .as_str()
        .unwrap()
        .contains("--insecure-bind"));
    let refused = repo.by_stdin(
        &[
            "init",
            "server",
            "--answers",
            "-",
            "--defaults",
            "--apply",
            "--json",
        ],
        &answers.to_string(),
    );
    assert_eq!(refused.status.code(), Some(1));
    let error: Value = serde_json::from_slice(&refused.stdout).unwrap();
    assert_eq!(error["error"]["kind"], json!("invalid_plan"));
    assert!(!repo.root.join(".branchyard").exists(), "nothing written");

    let answers = json!({
        "tenancy": "multi", "tenants": "acme, globex", "quota.max_running": "3",
        "webhook.url": "https://hooks.example.com/by", "secrets": [], "allow_delegation": "yes"
    });
    let out = repo.by_stdin(
        &[
            "init",
            "server",
            "--answers",
            "-",
            "--defaults",
            "--apply",
            "--json",
        ],
        &answers.to_string(),
    );
    assert!(out.status.success(), "{}", text(&out));
    let config: Value = serde_json::from_str(
        &fs::read_to_string(repo.root.join(".branchyard/server.json")).unwrap(),
    )
    .unwrap();
    let credentials = config["credentials"].as_array().unwrap();
    assert_eq!(credentials.len(), 2);
    for (credential, tenant) in credentials.iter().zip(["acme", "globex"]) {
        assert_eq!(credential["tenant"], json!(tenant));
        let token = fs::read_to_string(
            repo.root
                .join(format!(".branchyard/tokens/{tenant}-admin.token")),
        )
        .unwrap();
        assert_eq!(credential["token_sha256"].as_str().unwrap().len(), 64);
        assert!(
            !config.to_string().contains(token.trim()),
            "only the hash is stored"
        );
    }
    assert_eq!(config["tenants"]["acme"]["max_running"], json!(3));
    assert_eq!(config["allow_delegation"], json!(true));
    assert_eq!(
        config["webhooks"][0]["secret_file"],
        json!("tokens/webhook.secret")
    );
    assert!(
        fs::read_to_string(repo.root.join(".branchyard/tokens/.gitignore"))
            .unwrap()
            .contains('*')
    );
    let check = repo.by(&["serve", "--config", ".branchyard/server.json", "--check"]);
    assert!(check.status.success(), "{}", text(&check));
    repo.assert_no_secret_printed();
}

#[test]
fn apply_refuses_to_replace_a_different_file_without_force() {
    let repo = Repo::new();
    let mine = "# mine\nversion = 1\n[defaults]\nmax_turns = 3\n";
    fs::write(repo.root.join("branchyard.toml"), mine).unwrap();
    let out = repo.by(&["init", "project", "--defaults", "--apply", "--json"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    let error: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(error["error"]["kind"], json!("would_overwrite"));
    assert_eq!(error["error"]["paths"], json!(["branchyard.toml"]));
    assert_eq!(
        fs::read_to_string(repo.root.join("branchyard.toml")).unwrap(),
        mine
    );

    let dry = repo.json(&["init", "project", "--defaults", "--dry-run", "--json"]);
    let file = &dry["plan"]["files"][0];
    assert_eq!(file["overwrites"], json!(true));
    assert!(file["diff"].as_str().unwrap().contains("-# mine"));
    assert_eq!(
        dry["answers"]["max_turns"],
        json!(3),
        "the file's values are the defaults"
    );

    let out = repo.by(&[
        "init",
        "project",
        "--defaults",
        "--apply",
        "--force",
        "--json",
    ]);
    assert!(out.status.success(), "{}", text(&out));
    let written = fs::read_to_string(repo.root.join("branchyard.toml")).unwrap();
    assert!(written.contains("max_turns = 3"));
}

#[test]
fn a_pasted_secret_is_refused_and_never_echoed() {
    let repo = Repo::new();
    let answers = json!({
        "isolated": true, "secrets": ["ANTHROPIC_API_KEY"],
        "secret.ANTHROPIC_API_KEY": "sk-ant-api03-DONOTPRINTME0123456789abcdef"
    });
    let out = repo.by_stdin(
        &[
            "init",
            "project",
            "--answers",
            "-",
            "--defaults",
            "--apply",
            "--json",
        ],
        &answers.to_string(),
    );
    assert_eq!(out.status.code(), Some(1));
    let error: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(error["error"]["kind"], json!("incomplete"));
    assert!(!text(&out).contains("DONOTPRINTME"), "{}", text(&out));
    let next = repo.by_stdin(
        &["init", "project", "--answers", "-", "--json", "--next"],
        &answers.to_string(),
    );
    assert!(!text(&next).contains("DONOTPRINTME"));
    assert!(!repo.root.join("branchyard.toml").exists());
}

/// `by init project` on a machine with a device-code login in a file
/// (Codex's, holding a refresh token), Gemini CLI on `PATH` and
/// `GEMINI_API_KEY` set: it shows the login by where it is, records it as
/// `CODEX_AUTH` by that path when asked, writes the logged-in harnesses as
/// `[fleet.default]`, and gives the gateway a Gemini backend, with Claude
/// models routed to it; the file loads, and the login's content is never
/// printed or written.
#[test]
fn project_setup_records_a_login_a_fleet_and_gateway_backends_by_name() {
    let repo = Repo::new();
    let base = repo.root.parent().unwrap().to_path_buf();
    let (bin, home) = (base.join("bin"), base.join("home"));
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::create_dir_all(home.join(".local/bin")).unwrap();
    fs::create_dir_all(&bin).unwrap();
    let token = "rt-DONOTPRINT-0123456789abcdef";
    fs::write(
        home.join(".codex/auth.json"),
        format!(r#"{{"auth_mode": "chatgpt", "tokens": {{"refresh_token": "{token}"}}}}"#),
    )
    .unwrap();
    let gemini = bin.join("gemini");
    fs::write(&gemini, "#!/bin/sh\necho 0.63.0\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&gemini, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let run = |args: &[&str]| {
        let out = repo
            .command()
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    bin.display(),
                    repo.root.parent().unwrap().join("tools").display()
                ),
            )
            .env("BRANCHYARD_HARNESS_DIRS", "$HOME/.local/bin")
            .env("GEMINI_API_KEY", "not-a-key")
            .args(args)
            .output()
            .unwrap();
        repo.record(&out);
        out
    };
    let first = run(&["init", "project", "--json", "--next"]);
    let first: Value = serde_json::from_slice(&first.stdout).unwrap();
    let logins = first["facts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == "logins")
        .unwrap_or_else(|| panic!("{first:#}"));
    assert_eq!(
        logins["value"],
        "codex ~/.codex/auth.json (auth_mode chatgpt), as CODEX_AUTH"
    );
    let mut answers = Map::new();
    for (id, value) in [
        ("logins", "Codex login"),
        ("fleet", "Yes"),
        ("models", "Yes"),
        ("models.claude", "Yes"),
    ] {
        answers.insert(id.into(), json!(value));
    }
    let file = repo.write_answers(&answers);
    let out = run(&[
        "init",
        "project",
        "--answers",
        &file,
        "--defaults",
        "--apply",
        "--json",
    ]);
    assert!(out.status.success(), "{}", text(&out));
    let written = fs::read_to_string(repo.root.join("branchyard.toml")).unwrap();
    assert!(!written.contains(token), "{written}");
    assert!(
        written.contains("CODEX_AUTH = \"@~/.codex/auth.json\""),
        "{written}"
    );
    let config: toml_edit::DocumentMut = written.parse().unwrap();
    let fleet = &config["fleet"]["default"];
    assert_eq!(
        fleet["candidates"][0]["harness"].as_str(),
        Some("gemini-cli"),
        "{written}"
    );
    assert_eq!(fleet["judge"]["harness"].as_str(), Some("gemini-cli"));
    let backend = &config["models"]["backends"]["gemini"];
    assert_eq!(backend["api"].as_str(), Some("openai"), "{written}");
    assert_eq!(
        backend["url"].as_str(),
        Some("https://generativelanguage.googleapis.com/v1beta/openai")
    );
    assert_eq!(backend["key"].as_str(), Some("GEMINI_API_KEY"));
    assert_eq!(backend["compat"].as_str(), Some("gemini"));
    assert!(
        written.contains("backend_model = \"gemini-3.5-flash\""),
        "{written}"
    );
    let check = run(&["config", "validate"]);
    assert!(check.status.success(), "{}", text(&check));
    assert!(!repo.printed.borrow().contains(token));
}

#[test]
fn incomplete_answers_are_refused_with_what_remains() {
    let repo = Repo::new();
    let out = repo.by(&["init", "server", "--dry-run", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    let error: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(error["error"]["kind"], json!("incomplete"));
    assert!(error["error"]["paths"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p == "path: unanswered"));
}

#[test]
fn without_a_terminal_the_wizard_points_at_the_protocol() {
    let repo = Repo::new();
    let out = repo.by(&["init"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out).contains("by init project --json --next"),
        "{}",
        text(&out)
    );
    let topics = repo.json(&["init", "--json"]);
    let ids: Vec<&str> = topics["topics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["project", "server", "rig", "deploy", "plugin"]);
    for (args, error) in [
        (&["init", "nope", "--json", "--next"][..], "unknown topic"),
        (
            &["init", "project", "--next", "--apply"][..],
            "separate steps",
        ),
    ] {
        let out = repo.by(args);
        assert_eq!(out.status.code(), Some(2));
        assert!(text(&out).contains(error), "{}", text(&out));
    }
    assert!(text(&repo.by(&["help", "init"])).contains("--next"));
}

#[test]
fn the_configuration_supplies_defaults_under_flags_and_variables() {
    let repo = Repo::new();
    fs::write(
        repo.root.join("branchyard.toml"),
        "version = 1\n[remote]\nurl = \"http://127.0.0.1:9\"\ntoken_file = \"nowhere.token\"\n[serve]\nconfig = \"missing-server.json\"\n",
    )
    .unwrap();
    // [remote] makes `by ls` go to that server, with that token file.
    let out = repo.by(&["ls"]);
    assert!(!out.status.success());
    assert!(text(&out).contains("nowhere.token"), "{}", text(&out));
    // A flag wins.
    let out = repo.by(&["--remote", "http://127.0.0.1:10", "ls"]);
    assert!(!text(&out).contains("nowhere.token"), "{}", text(&out));
    // [serve] config is `by serve`'s --config.
    let out = repo.by(&["serve", "--check"]);
    assert!(text(&out).contains("missing-server.json"), "{}", text(&out));
    // Inside a harness's branch the file is not read.
    let out = repo
        .command()
        .env("BRANCHYARD_BRANCH", "x")
        .args(["config", "show"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let out = repo
        .command()
        .env("BRANCHYARD_BRANCH", "x")
        .arg("ls")
        .output()
        .unwrap();
    assert!(!text(&out).contains("nowhere.token"), "{}", text(&out));

    let shown = repo.json(&["config", "show", "--json"]);
    assert_eq!(
        shown["values"]["remote.url"]["source"]["kind"],
        json!("project")
    );
    let out = repo
        .command()
        .env("BRANCHYARD_REMOTE", "http://127.0.0.1:11")
        .args(["config", "show", "--json"])
        .output()
        .unwrap();
    let shown: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        shown["values"]["remote.url"]["source"],
        json!({"kind": "env", "var": "BRANCHYARD_REMOTE"})
    );

    // A broken file stops every command, naming the file and line.
    fs::write(
        repo.root.join("branchyard.toml"),
        "version = 1\n[defaults]\nharnes = \"codex\"\n",
    )
    .unwrap();
    let out = repo.by(&["ls"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out).contains("branchyard.toml: line 3"),
        "{}",
        text(&out)
    );
    let out = repo.by(&["config", "validate", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["valid"], json!(false));
}

/// The wizard itself, on a pseudo-terminal from util-linux `script`,
/// accepting every default with Enter: it shows what it detected, asks,
/// reviews the diff, and writes only after the last confirmation.
#[cfg(target_os = "linux")]
#[test]
fn the_wizard_accepts_defaults_on_a_terminal_and_writes_after_review() {
    if !Path::new("/usr/bin/script").exists() {
        eprintln!("skipped: no /usr/bin/script for a pseudo-terminal");
        return;
    }
    let repo = Repo::new();
    let by = env!("CARGO_BIN_EXE_by");
    let mut run = Command::new("/usr/bin/script");
    branchyard_testkit::hermetic(&mut run)
        .args(["-qfc", &format!("{by} init project"), "/dev/null"])
        .current_dir(&repo.root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("HOME", repo.root.parent().unwrap().join("home"))
        .env(
            "BRANCHYARD_USER_CONFIG",
            repo.root.parent().unwrap().join("user.toml"),
        )
        .env("TERM", "xterm")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut process = run.spawn().unwrap();
    process
        .stdin
        .take()
        .unwrap()
        .write_all("\r".repeat(40).as_bytes())
        .unwrap();
    let out = process.wait_with_output().unwrap();
    let screen = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{screen}");
    for expected in [
        "by init",
        "Repository",
        "Which harness should new branches use?",
        "Review",
        "wrote branchyard.toml",
    ] {
        assert!(screen.contains(expected), "{expected:?} missing:\n{screen}");
    }
    let written = fs::read_to_string(repo.root.join("branchyard.toml")).unwrap();
    assert!(written.contains("check = \"cargo test\""), "{written}");
}

/// `by init project` reads a repository's `devcontainer.json` into a
/// prepared `[workspace]` with the variables its processes get, and the
/// file it writes loads and validates.
#[test]
fn project_setup_imports_a_devcontainer_as_a_prepared_workspace() {
    let repo = Repo::new();
    fs::create_dir_all(repo.root.join(".devcontainer")).unwrap();
    fs::write(
        repo.root.join(".devcontainer/devcontainer.json"),
        r#"{
  "image": "mcr.microsoft.com/devcontainers/base:ubuntu", // the image
  "containerEnv": { "APP_ENV": "dev" },
  "remoteEnv": { "TOKEN_FILE": "${localEnv:HOME}/.token" },
  "onCreateCommand": "make deps",
  "postCreateCommand": ["make", "generate"],
  "postStartCommand": "make serve",
}"#,
    )
    .unwrap();
    fs::write(repo.root.join("package.json"), "{}").unwrap();
    let out = repo.by(&["init", "project", "--defaults", "--apply"]);
    assert!(out.status.success(), "{}", text(&out));
    let written = fs::read_to_string(repo.root.join("branchyard.toml")).unwrap();
    let config: toml_edit::DocumentMut = written.parse().unwrap();
    let workspace = &config["workspace"];
    assert_eq!(
        workspace["setup"].as_str(),
        Some("make deps && make generate"),
        "{written}"
    );
    assert_eq!(workspace["prepare"].as_bool(), Some(true), "{written}");
    let inputs: Vec<&str> = workspace["inputs"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(inputs, [".devcontainer/devcontainer.json", "package.json"]);
    assert_eq!(workspace["env"]["APP_ENV"].as_str(), Some("dev"));
    assert_eq!(
        workspace["env"]["TOKEN_FILE"].as_str(),
        Some("${HOME}/.token")
    );
    let runs = workspace["run"].as_table_like().unwrap();
    let (_, script) = runs.iter().next().unwrap();
    assert_eq!(script["command"].as_str(), Some("make serve"));
    let check = repo.by(&["config", "validate"]);
    assert!(check.status.success(), "{}", text(&check));
}

#[test]
fn by_init_plugin_lists_previews_and_installs_a_harness_once() {
    let repo = Repo::new();
    let base = repo.root.parent().unwrap().to_path_buf();
    let bin = base.join("bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(bin.join("claude"), "#!/bin/sh\necho 2.1.0\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(bin.join("claude"), fs::Permissions::from_mode(0o755)).unwrap();
    }
    let home = base.join("home");
    fs::create_dir_all(home.join(".claude")).unwrap();
    let settings = home.join(".claude/settings.json");
    fs::write(&settings, "{\n    \"model\": \"opus\"\n}\n").unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        repo.root.parent().unwrap().join("tools").display()
    );
    let by = |args: &[&str]| {
        let out = repo
            .command()
            .env("PATH", &path)
            .args(args)
            .output()
            .unwrap();
        repo.record(&out);
        out
    };
    // The listing: detected first, as text and JSON.
    let listed = by(&["init", "plugin"]);
    assert!(listed.status.success(), "{}", text(&listed));
    assert!(text(&listed).contains("Claude Code (claude-code)  detected (claude)"));
    let list: Value = serde_json::from_slice(&by(&["init", "plugin", "--json"]).stdout).unwrap();
    assert_eq!(list["harnesses"][0]["harness"], "claude-code");
    assert_eq!(list["harnesses"][0]["detected"], true);
    // Without a terminal: the diffs, then nothing written.
    let preview = by(&["init", "plugin", "--all-detected"]);
    assert!(preview.status.success(), "{}", text(&preview));
    assert!(text(&preview).contains("merge-json"), "{}", text(&preview));
    // What each gets, diffs named under the home, and the command that writes them.
    let shown = text(&preview);
    assert!(
        shown.contains(
            "Claude Code: 2 skills, 3 commands, 2 hooks, status line in ~/.claude/settings.json"
        ),
        "{shown}"
    );
    assert!(
        shown.contains("+++ b/.claude/skills/setup/SKILL.md") && !shown.contains("b//"),
        "{shown}"
    );
    assert!(
        shown.trim_end().ends_with(
            "by init plugin --all-detected --apply\n      Write these files; nothing has been written yet."
        ),
        "{shown}"
    );
    assert_eq!(
        fs::read_to_string(&settings).unwrap(),
        "{\n    \"model\": \"opus\"\n}\n"
    );
    // The preview of the named harness ends with its own write command.
    let named = by(&["init", "plugin", "claude-code", "--dry-run"]);
    assert!(named.status.success(), "{}", text(&named));
    assert!(
        text(&named).contains("by init plugin claude-code --apply\n"),
        "{}",
        text(&named)
    );
    // --apply writes, keeps the person's settings and prints the notes.
    let applied = by(&["init", "plugin", "claude-code", "--apply"]);
    assert!(applied.status.success(), "{}", text(&applied));
    assert!(
        text(&applied).contains("note: Claude Code:"),
        "{}",
        text(&applied)
    );
    // After writing: the repository next.
    assert!(
        text(&applied).trim_end().contains("Next:\n  by setup\n"),
        "{}",
        text(&applied)
    );
    let merged: Value = serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(merged["model"], "opus");
    assert_eq!(merged["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
    assert_eq!(merged["statusLine"]["type"], "command");
    assert!(home.join(".claude/skills/setup/SKILL.md").exists());
    // A second run has nothing to do.
    let again = by(&["init", "plugin", "claude-code"]);
    assert!(
        text(&again).contains("nothing to write"),
        "{}",
        text(&again)
    );
    // A clash is refused and writes nothing; an unknown harness is named.
    fs::write(&settings, "{\"hooks\": []}\n").unwrap();
    let refused = by(&["init", "plugin", "claude-code", "--apply"]);
    assert!(!refused.status.success());
    assert!(
        text(&refused).contains("failed validation"),
        "{}",
        text(&refused)
    );
    assert_eq!(fs::read_to_string(&settings).unwrap(), "{\"hooks\": []}\n");
    let unknown = by(&["init", "plugin", "nope"]);
    assert!(text(&unknown).contains("no install for harness \"nope\""));
}

/// An executable `name` that prints `body` in `dir`.
fn fake_program(dir: &Path, name: &str, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join(name), format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(dir.join(name), fs::Permissions::from_mode(0o755)).unwrap();
}

const STALE: &str =
    "version = 1\nagent = \"claude\"\nbudget = 5\n[workspace]\nsetup = \"npm ci\"\n";

/// A `branchyard.toml` that `by` refuses is named, not quietly replaced by
/// a diff that deletes its keys; `--apply` says it does not load.
#[test]
fn a_configuration_that_does_not_load_is_named_and_force_replaces_it() {
    let repo = Repo::new();
    fs::write(repo.root.join("branchyard.toml"), STALE).unwrap();

    // The facts and the plan both say so, with the line and the new name.
    let next = repo.json(&["init", "project", "--json", "--next"]);
    let fact = next["facts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == "config_problem")
        .unwrap_or_else(|| panic!("{next:#}"));
    assert_eq!(fact["label"], "branchyard.toml");
    assert!(
        fact["value"]
            .as_str()
            .unwrap()
            .contains("line 2: unknown field `agent` (did you mean `harness`, under [defaults]?)"),
        "{fact:#}"
    );
    let dry = repo.by(&["init", "project", "--defaults", "--dry-run"]);
    assert!(dry.status.success(), "{}", text(&dry));
    let shown = text(&dry);
    assert!(
        shown.contains("note: branchyard.toml does not load: line 2: unknown field `agent`"),
        "{shown}"
    );
    assert!(shown.contains("its values are not used here"), "{shown}");

    // --apply: not "different content".
    let refused = repo.by(&["init", "project", "--defaults", "--apply"]);
    assert_eq!(refused.status.code(), Some(1));
    let message = text(&refused);
    assert!(
        message.contains("branchyard.toml does not load (line 2); --force replaces it"),
        "{message}"
    );
    assert!(!message.contains("different content"), "{message}");
    assert_eq!(
        fs::read_to_string(repo.root.join("branchyard.toml")).unwrap(),
        STALE,
        "nothing was written"
    );
    let json = repo.by(&["init", "project", "--defaults", "--apply", "--json"]);
    assert_eq!(json.status.code(), Some(1));
    let error: Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(error["error"]["kind"], "would_overwrite");
    assert_eq!(error["error"]["paths"], json!(["branchyard.toml"]));

    let forced = repo.by(&["init", "project", "--defaults", "--apply", "--force"]);
    assert!(forced.status.success(), "{}", text(&forced));
    let check = repo.by(&["config", "validate"]);
    assert!(check.status.success(), "{}", text(&check));
}

/// A run script is proposed only for a script `package.json` has, and the
/// question is not asked otherwise.
#[test]
fn a_run_script_is_proposed_only_for_a_package_json_script() {
    let without = Repo::new();
    // Only the package: Cargo.toml would add `cargo fetch` to its setup.
    fs::remove_file(without.root.join("Cargo.toml")).unwrap();
    fs::write(
        without.root.join("package.json"),
        r#"{"name": "app", "scripts": {"test": "node --test"}}"#,
    )
    .unwrap();
    fs::write(without.root.join("package-lock.json"), "{}").unwrap();
    let (answers, _) = walk(&without, "project");
    assert!(answers.contains_key("workspace.setup"), "{answers:?}");
    assert!(!answers.contains_key("workspace.run"), "{answers:?}");
    let out = without.by(&["init", "project", "--defaults", "--apply"]);
    assert!(out.status.success(), "{}", text(&out));
    let written = fs::read_to_string(without.root.join("branchyard.toml")).unwrap();
    assert!(written.contains("setup = \"npm ci\""), "{written}");
    assert!(!written.contains("workspace.run"), "{written}");
    assert!(!written.contains("npm run dev"), "{written}");

    let with = Repo::new();
    fs::remove_file(with.root.join("Cargo.toml")).unwrap();
    fs::write(
        with.root.join("package.json"),
        r#"{"name": "app", "scripts": {"start": "node server.js"}}"#,
    )
    .unwrap();
    fs::write(with.root.join("pnpm-lock.yaml"), "").unwrap();
    let out = with.by(&["init", "project", "--defaults", "--apply"]);
    assert!(out.status.success(), "{}", text(&out));
    let written = fs::read_to_string(with.root.join("branchyard.toml")).unwrap();
    assert!(
        written.contains("command = \"PORT=$BRANCHYARD_PORT pnpm start\""),
        "{written}"
    );

    // Scripts that are new lead the closing list with the trust they need.
    let shown = text(&out);
    let trust = shown.find("by workspace trust").unwrap_or(usize::MAX);
    let validate = shown.find("by config validate").unwrap_or(0);
    assert!(trust < validate, "{shown}");
    assert!(shown.contains("Your first `by run` needs this"), "{shown}");
}

/// `by init project` and `by up` need a repository, and say how to get one;
/// from a subdirectory they find the root.
#[test]
fn project_setup_and_up_refuse_outside_a_repository() {
    let repo = Repo::new();
    let plain = repo.root.parent().unwrap().join("plain");
    fs::create_dir_all(&plain).unwrap();
    let at = |args: &[&str]| {
        let out = repo
            .command()
            .current_dir(&plain)
            .args(args)
            .output()
            .unwrap();
        repo.record(&out);
        out
    };
    let wanted = format!(
        "not a git repository: {}. Run `git init` here, or cd into one",
        fs::canonicalize(&plain).unwrap().display()
    );
    for args in [
        &["init", "project", "--defaults", "--apply"][..],
        &["init", "project", "--json", "--next"][..],
    ] {
        let out = at(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", text(&out));
        assert!(text(&out).contains(&wanted), "{args:?}: {}", text(&out));
    }
    assert!(!plain.join("branchyard.toml").exists());
    let json = at(&["init", "project", "--json", "--dry-run"]);
    let error: Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(error["error"]["kind"], "not_a_repository");
    assert_eq!(error["error"]["message"], json!(wanted));
    // Topics that write nothing a repository owns are not refused.
    let listing = at(&["init", "plugin"]);
    assert!(listing.status.success(), "{}", text(&listing));

    let up = at(&["up", "--plan"]);
    assert_eq!(up.status.code(), Some(1));
    assert!(text(&up).contains(&wanted), "{}", text(&up));
    assert!(
        !text(&up).contains("is not inside a git work tree"),
        "{}",
        text(&up)
    );

    // From a subdirectory of a repository, the file lands at its root.
    let deep = repo.root.join("src/deep");
    fs::create_dir_all(&deep).unwrap();
    let out = repo
        .command()
        .current_dir(&deep)
        .args(["init", "project", "--defaults", "--apply"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert!(repo.root.join("branchyard.toml").exists());
    assert!(!deep.join("branchyard.toml").exists());
}

/// `by init plugin` with nothing named, outside the protocol, says what to
/// name instead of listing a question's internal id.
#[test]
fn by_init_plugin_with_no_names_says_what_to_name() {
    let repo = Repo::new();
    for step in ["--dry-run", "--apply"] {
        let out = repo.by(&["init", "plugin", step]);
        assert_eq!(out.status.code(), Some(2), "{}", text(&out));
        let said = text(&out);
        assert!(
            said.contains("name harness IDs or --all-detected; `by init plugin` lists them"),
            "{said}"
        );
        assert!(
            !said.contains("unanswered") && !said.contains("host"),
            "{said}"
        );
    }
    let json = repo.by(&["init", "plugin", "--dry-run", "--json"]);
    assert_eq!(json.status.code(), Some(2));
    let error: Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(error["error"]["kind"], "usage");
    // Answers or --defaults carry the choice, as the protocol does.
    let defaults = repo.json(&["init", "plugin", "--defaults", "--dry-run", "--json"]);
    assert_eq!(defaults["done"], true, "{defaults:#}");
    let first = repo.json(&["init", "plugin", "--json", "--next"]);
    let ids: Vec<&str> = first["questions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|q| q["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["harnesses"]);
}

/// Claude Code's status line is merged when there is none; one you have is
/// kept, with the line to choose, and the install still goes through.
#[test]
fn claude_codes_status_line_is_merged_when_absent_and_kept_when_present() {
    let repo = Repo::new();
    let base = repo.root.parent().unwrap().to_path_buf();
    let bin = base.join("bin");
    fake_program(&bin, "claude", "echo 2.1.0");
    let home = base.join("home");
    fs::create_dir_all(home.join(".claude")).unwrap();
    let settings = home.join(".claude/settings.json");
    let path = format!(
        "{}:{}",
        bin.display(),
        repo.root.parent().unwrap().join("tools").display()
    );
    let by = |args: &[&str]| {
        let out = repo
            .command()
            .env("PATH", &path)
            .args(args)
            .output()
            .unwrap();
        repo.record(&out);
        out
    };
    let settings_json =
        || -> Value { serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap() };

    // None yet: merged beside the hooks, and no note about it.
    fs::write(&settings, "{\n  \"model\": \"opus\"\n}\n").unwrap();
    // A dry run with --defaults (the detected harnesses) ends with the command that writes it.
    let dry = by(&["init", "plugin", "--defaults", "--dry-run"]);
    assert!(dry.status.success(), "{}", text(&dry));
    assert!(
        text(&dry).trim_end().ends_with(
            "by init plugin --defaults --apply\n      Write these files; nothing has been written yet."
        ),
        "{}",
        text(&dry)
    );
    let applied = by(&["init", "plugin", "claude-code", "--apply"]);
    assert!(applied.status.success(), "{}", text(&applied));
    let merged = settings_json();
    assert_eq!(
        merged["statusLine"]["command"],
        "command -v by >/dev/null 2>&1 && by statusline --harness claude-code || true"
    );
    assert_eq!(merged["model"], "opus");
    assert!(!text(&applied).contains("kept the"), "{}", text(&applied));

    // Yours: kept, hooks merged, the note says what to set instead.
    let mine = "{\n  \"statusLine\": {\"type\": \"command\", \"command\": \"ccstatusline\"}\n}\n";
    fs::write(&settings, mine).unwrap();
    let preview = by(&["init", "plugin", "claude-code"]);
    let shown = text(&preview);
    assert!(preview.status.success(), "{shown}");
    assert!(
        shown.contains("Claude Code: 2 skills, 3 commands, 2 hooks in ~/.claude/settings.json"),
        "{shown}"
    );
    assert!(
        shown.contains("kept the \"statusLine\" already in ~/.claude/settings.json."),
        "{shown}"
    );
    let applied = by(&["init", "plugin", "claude-code", "--apply"]);
    assert!(applied.status.success(), "{}", text(&applied));
    let merged = settings_json();
    assert_eq!(merged["statusLine"]["command"], "ccstatusline");
    assert_eq!(merged["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
    // And a second run has nothing to do, and offers no command to write it.
    let again = by(&["init", "plugin", "claude-code"]);
    assert!(
        text(&again).contains("nothing to write"),
        "{}",
        text(&again)
    );
    let dry = by(&["init", "plugin", "claude-code", "--dry-run"]);
    assert!(dry.status.success(), "{}", text(&dry));
    assert!(
        text(&dry).contains("Everything is already installed; nothing to write."),
        "{}",
        text(&dry)
    );
    assert!(!text(&dry).contains("--apply"), "{}", text(&dry));
}

/// On a terminal `by init plugin` lists the harnesses, asks which (the ones
/// found on PATH are chosen), reviews and writes: the files naming them
/// writes, not a different, skills-only install.
#[test]
fn the_plugin_wizard_asks_which_harnesses_and_writes_what_naming_them_would() {
    if !Path::new("/usr/bin/script").exists() {
        eprintln!("skipped: no /usr/bin/script for a pseudo-terminal");
        return;
    }
    let repo = Repo::new();
    let base = repo.root.parent().unwrap().to_path_buf();
    let bin = base.join("bin");
    fake_program(&bin, "claude", "echo 2.1.0");
    let by = env!("CARGO_BIN_EXE_by");
    let mut run = Command::new("/usr/bin/script");
    branchyard_testkit::hermetic(&mut run)
        .args(["-qfc", &format!("{by} init plugin"), "/dev/null"])
        .current_dir(&repo.root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("HOME", base.join("home"))
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                repo.root.parent().unwrap().join("tools").display()
            ),
        )
        .env("TERM", "xterm")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut process = run.spawn().unwrap();
    process
        .stdin
        .take()
        .unwrap()
        .write_all("\r".repeat(10).as_bytes())
        .unwrap();
    let out = process.wait_with_output().unwrap();
    let screen = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{screen}");
    for expected in [
        "Hooks and skills for your coding harnesses",
        "Which harnesses should get Branchyard's hooks, commands and skills?",
        "2 skills, 3 commands, 2 hooks",
        "wrote",
        "by setup",
    ] {
        assert!(screen.contains(expected), "{expected:?} missing:\n{screen}");
    }
    let home = base.join("home");
    assert!(home.join(".claude/skills/setup/SKILL.md").exists());
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(home.join(".claude/settings.json")).unwrap())
            .unwrap();
    assert!(settings["hooks"]["SessionStart"].is_array(), "{settings:#}");
    assert_eq!(settings["statusLine"]["type"], "command");
    assert!(
        !home.join(".codex").exists(),
        "only the detected harness is chosen"
    );
}
