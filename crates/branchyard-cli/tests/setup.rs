//! `by init` and `by config` end to end: the built binary in temporary
//! repositories, driven the way a harness drives the protocol. Hermetic:
//! the user configuration is redirected, no model is called, and every
//! generated secret is checked to appear in no output.

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
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("app")).unwrap();
        let root = fs::canonicalize(dir.join("app")).unwrap();
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
        command
            .current_dir(&self.root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("NO_COLOR", "1")
            .env("HOME", self.root.parent().unwrap().join("home"))
            .env(
                "BRANCHYARD_USER_CONFIG",
                self.root.parent().unwrap().join("user.toml"),
            )
            .stdin(Stdio::null());
        for var in [
            "BRANCHYARD_BRANCH",
            "BRANCHYARD_REMOTE",
            "BRANCHYARD_TOKEN_FILE",
            "BRANCHYARD_REPO",
            "BRANCHYARD_CA_FILE",
            "XDG_CONFIG_HOME",
        ] {
            command.env_remove(var);
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
    run.args(["-qfc", &format!("{by} init project"), "/dev/null"])
        .current_dir(&repo.root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("HOME", repo.root.parent().unwrap().join("home"))
        .env(
            "BRANCHYARD_USER_CONFIG",
            repo.root.parent().unwrap().join("user.toml"),
        )
        .env("TERM", "xterm")
        .env_remove("BRANCHYARD_REMOTE")
        .env_remove("BRANCHYARD_BRANCH")
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
