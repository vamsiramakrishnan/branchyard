//! `by doctor` through the built `by`, against temporary repositories: a
//! repository without `.branchyard/` fails with the init hint and is not
//! changed by the report, an initialized one has no failure, a refused
//! `branchyard.toml` is shown with its line, untrusted `[workspace]` scripts
//! warn until trusted and again after a change, a store written by another
//! `by` is reported and left as it is, a configured server is asked once,
//! the model gateway's keys are named and never shown, and the exit status
//! is 0 without a failure, 1 with one, and 2 for a usage error. Hermetic;
//! requires `git`, `sh` and `python3`.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use branchyard_testkit::{MockHttp, Response};
use serde_json::{json, Value};

/// The kit's repository, plus what this file adds.
struct Repo(branchyard_testkit::Repo);

impl std::ops::Deref for Repo {
    type Target = branchyard_testkit::Repo;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// What `by doctor --json` printed, and how it exited.
struct Report {
    code: Option<i32>,
    value: Value,
    stdout: String,
}

impl Report {
    /// The row named `name`; fails the test, listing the names, if none is.
    fn check(&self, name: &str) -> &Value {
        let rows = self.value["checks"].as_array().unwrap();
        rows.iter().find(|c| c["name"] == name).unwrap_or_else(|| {
            let names: Vec<&str> = rows.iter().filter_map(|c| c["name"].as_str()).collect();
            panic!("no check {name} in {names:?}")
        })
    }

    fn status(&self, name: &str) -> &str {
        self.check(name)["status"].as_str().unwrap()
    }

    fn detail(&self, name: &str) -> &str {
        self.check(name)["detail"].as_str().unwrap()
    }

    fn fix(&self, name: &str) -> &str {
        self.check(name)["fix"].as_str().unwrap_or_default()
    }

    fn overall(&self) -> &str {
        self.value["status"].as_str().unwrap()
    }
}

impl Repo {
    fn new() -> Repo {
        Repo(branchyard_testkit::repo!())
    }

    /// A repository `by` has been used in.
    fn used() -> Repo {
        let repo = Repo::new();
        repo.ok(&["ls"]);
        repo
    }

    fn project_file(&self, text: &str) {
        fs::write(self.root.join("branchyard.toml"), text).unwrap();
    }

    fn user_file(&self, text: &str) {
        let path = self.dir.join("user/config.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// `by doctor --json <args>` in the repository.
    fn doctor(&self, args: &[&str]) -> Report {
        self.doctor_in(&self.root, args, &[])
    }

    /// `by doctor --json <args>` run in `dir`, with `envs` set.
    fn doctor_in(&self, dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Report {
        let mut command = self.command(self.by_path());
        command
            .current_dir(dir)
            .arg("doctor")
            .arg("--json")
            .args(args);
        for (key, value) in envs {
            command.env(key, value);
        }
        let out = command.output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
            panic!(
                "by doctor did not print JSON ({e})\n{stdout}\n{}",
                String::from_utf8_lossy(&out.stderr)
            )
        });
        Report {
            code: out.status.code(),
            value,
            stdout,
        }
    }

    /// `by doctor <args>` as text.
    fn doctor_text(&self, args: &[&str]) -> Output {
        let mut all = vec!["doctor"];
        all.extend(args);
        self.by(&all)
    }

    fn state_db(&self) -> std::path::PathBuf {
        self.root.join(".branchyard/state.db")
    }
}

/// Run `script` with python3, `args` after it; its standard output.
fn python(script: &str, args: &[&Path]) -> String {
    let out = Command::new("python3")
        .args(["-c", script])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn schema_of(db: &Path) -> String {
    python(
        "import sqlite3,sys; print(sqlite3.connect(sys.argv[1]).execute(\"SELECT value FROM meta WHERE key='schema'\").fetchone()[0])",
        &[db],
    )
}

fn set_schema(db: &Path, version: &str) {
    python(
        &format!(
            "import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); c.execute(\"UPDATE meta SET value='{version}' WHERE key='schema'\"); c.commit()"
        ),
        &[db],
    );
}

#[test]
fn a_repository_without_branchyard_fails_with_the_init_hint_and_is_left_alone() {
    let repo = Repo::new();
    let report = repo.doctor(&[]);
    assert_eq!(report.code, Some(1), "{}", report.stdout);
    assert_eq!(report.overall(), "fail");
    assert_eq!(report.status("repository.git"), "ok");
    assert!(report.detail("repository.git").contains("on branch main"));
    assert_eq!(report.status("repository.state"), "fail");
    assert!(report
        .detail("repository.state")
        .contains(".branchyard does not exist"));
    assert!(report.fix("repository.state").contains("by init"));
    // Nothing to read yet is not a failure of the checks that read it.
    assert_eq!(report.status("repository.schema"), "ok");
    assert_eq!(report.status("repository.services"), "ok");
    // The report created nothing: no .branchyard/, and no exclude entry.
    assert!(!repo.root.join(".branchyard").exists());
    let exclude = repo.root.join(".git/info/exclude");
    assert!(!fs::read_to_string(exclude)
        .unwrap_or_default()
        .contains(".branchyard"));
    // The text carries the same, with what to do.
    let text = repo.doctor_text(&[]);
    assert_eq!(text.status.code(), Some(1));
    let text = String::from_utf8_lossy(&text.stdout);
    assert!(text.contains("fail    repository.state"), "{text}");
    assert!(text.contains("To do:\n"), "{text}");
    assert!(text.contains("repository.state: run `by init`"), "{text}");
    let last = text.lines().last().unwrap();
    assert!(last.starts_with("fail: 1 failed"), "{text}");
}

#[test]
fn an_initialized_repository_has_no_failure() {
    let repo = Repo::used();
    let report = repo.doctor(&[]);
    assert_eq!(report.code, Some(0), "{}", report.stdout);
    assert_ne!(report.overall(), "fail", "{}", report.stdout);
    assert_eq!(report.status("repository.git"), "ok");
    assert_eq!(report.status("repository.state"), "ok");
    assert!(report.detail("repository.state").contains("0 branches"));
    assert_eq!(report.status("repository.schema"), "ok");
    assert!(report.detail("repository.schema").starts_with("schema 2"));
    assert_eq!(report.status("repository.services"), "ok");
    assert_eq!(report.status("config.merged"), "ok");
    assert_eq!(report.status("server.reachable"), "ok");
    assert!(report
        .detail("server.reachable")
        .starts_with("not configured"));
    assert_eq!(report.status("models.keys"), "ok");
}

#[test]
fn a_broken_branchyard_toml_fails_naming_the_file_and_the_line() {
    let repo = Repo::used();
    repo.project_file("version = 1\n[defaults]\nharnes = \"codex\"\n");
    let report = repo.doctor(&[]);
    assert_eq!(report.code, Some(1), "{}", report.stdout);
    assert_eq!(report.status("config.project"), "fail");
    let detail = report.detail("config.project");
    assert!(detail.contains("branchyard.toml: line 3"), "{detail}");
    assert!(detail.contains("harnes"), "{detail}");
    assert!(report.fix("config.project").contains("by config validate"));
    // What depends on the configuration says so instead of guessing.
    assert_eq!(report.status("config.merged"), "warn");
    assert!(report.detail("config.merged").starts_with("not checked: "));
    assert_eq!(report.status("harnesses.default"), "warn");
    // The user file is shown the same way, and both are reported.
    repo.user_file("[remote]\nurl = 3\n");
    let report = repo.doctor(&[]);
    assert_eq!(report.status("config.user"), "fail");
    assert!(report.detail("config.user").contains("config.toml: line 2"));
    assert_eq!(report.status("config.project"), "fail");
    // The text shows the refusal whole.
    let text = repo.doctor_text(&[]);
    let text = String::from_utf8_lossy(&text.stdout);
    assert!(text.contains("branchyard.toml: line 3"), "{text}");
}

#[test]
fn untrusted_workspace_scripts_warn_until_trusted_and_again_after_a_change() {
    let repo = Repo::used();
    repo.project_file("[workspace]\nsetup = \"echo one\"\n");
    let report = repo.doctor(&[]);
    assert_eq!(
        report.code,
        Some(0),
        "a warning does not fail: {}",
        report.stdout
    );
    assert_eq!(report.status("config.workspace_trust"), "warn");
    assert!(report
        .detail("config.workspace_trust")
        .contains("have not trusted"));
    assert!(report
        .fix("config.workspace_trust")
        .contains("by workspace trust"));
    assert_eq!(report.overall(), "warn");

    repo.ok(&["workspace", "trust"]);
    let report = repo.doctor(&[]);
    assert_eq!(report.status("config.workspace_trust"), "ok");
    assert!(report
        .detail("config.workspace_trust")
        .contains("matches the one you trusted"));

    repo.project_file("[workspace]\nsetup = \"echo two\"\n");
    let report = repo.doctor(&[]);
    assert_eq!(report.status("config.workspace_trust"), "warn");
    assert!(report
        .detail("config.workspace_trust")
        .contains("changed since you trusted it"));
    assert!(report
        .detail("config.workspace_trust")
        .contains("trusted digest"));

    // Copy globs alone run nothing, so they need no trust.
    repo.project_file("[workspace]\ncopy = [\".env\"]\n");
    let report = repo.doctor(&[]);
    assert_eq!(report.status("config.workspace_trust"), "ok");
    assert!(report
        .detail("config.workspace_trust")
        .contains("needs no trust"));
}

#[test]
fn the_json_has_the_documented_shape_and_every_row_that_is_not_ok_says_what_to_do() {
    let repo = Repo::new();
    let report = repo.doctor(&[]);
    let object = report.value.as_object().unwrap();
    assert_eq!(object.len(), 2, "{object:?}");
    assert!(["ok", "warn", "fail"].contains(&report.overall()));
    let rows = report.value["checks"].as_array().unwrap();
    assert!(rows.len() >= 15, "{} checks", rows.len());
    let mut names = Vec::new();
    for row in rows {
        let row = row.as_object().unwrap();
        assert_eq!(row.len(), 4, "{row:?}");
        let name = row["name"].as_str().unwrap();
        assert!(name.contains('.') && !name.contains(' '), "{name}");
        names.push(name);
        let status = row["status"].as_str().unwrap();
        assert!(["ok", "warn", "fail"].contains(&status), "{status}");
        let detail = row["detail"].as_str().unwrap();
        assert!(
            !detail.is_empty() && !detail.contains('\n'),
            "{name}: {detail:?}"
        );
        match (status, &row["fix"]) {
            ("ok", Value::Null) => {}
            ("ok", fix) => panic!("{name} is ok but says what to do: {fix}"),
            (_, Value::String(fix)) => assert!(!fix.is_empty() && !fix.contains('\n'), "{name}"),
            (_, fix) => panic!("{name} is {status} with no fix: {fix}"),
        }
    }
    let mut unique = names.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), names.len(), "check names repeat: {names:?}");
    // The overall status is the worst row.
    let worst = ["ok", "warn", "fail"]
        .iter()
        .rposition(|s| rows.iter().any(|r| r["status"] == *s))
        .unwrap();
    assert_eq!(report.overall(), ["ok", "warn", "fail"][worst]);
    // Report order: the repository first, the model gateway last.
    assert_eq!(names.first(), Some(&"repository.git"));
    assert_eq!(names.last(), Some(&"models.prices"));
}

#[test]
fn the_exit_status_is_zero_without_a_failure_one_with_one_and_two_for_a_usage_error() {
    let repo = Repo::used();
    assert_eq!(repo.doctor_text(&[]).status.code(), Some(0));
    repo.project_file("[workspace]\nsetup = \"echo one\"\n");
    assert_eq!(
        repo.doctor_text(&[]).status.code(),
        Some(0),
        "a warning is not a failure"
    );
    repo.project_file("not toml at all [");
    assert_eq!(repo.doctor_text(&[]).status.code(), Some(1));
    let out = repo.by(&["doctor", "--no-such-flag"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--no-such-flag"));
    // The report is on stdout, nothing on stderr.
    let out = repo.doctor_text(&[]);
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
}

#[test]
fn outside_a_repository_it_fails_and_path_points_it_at_one() {
    let repo = Repo::used();
    // The scratch directory above the repository is not in one.
    let report = repo.doctor_in(&repo.dir, &[], &[]);
    assert_eq!(report.code, Some(1), "{}", report.stdout);
    assert_eq!(report.status("repository.git"), "fail");
    assert!(report
        .detail("repository.git")
        .contains("not inside a git repository"));
    assert!(report.fix("repository.git").contains("--path"));
    assert_eq!(report.status("repository.state"), "warn");
    assert!(report.fix("repository.state").contains("repository.git"));
    let text = repo
        .command(repo.by_path())
        .current_dir(&repo.dir)
        .arg("doctor")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&text.stdout);
    // The rows that only point at the failed one are not listed again.
    assert_eq!(
        text.matches("fix repository.git first").count(),
        0,
        "{text}"
    );

    let path = repo.root.display().to_string();
    let report = repo.doctor_in(&repo.dir, &["--path", &path], &[]);
    assert_eq!(report.code, Some(0), "{}", report.stdout);
    assert_eq!(report.status("repository.state"), "ok");
    // A path that is not a directory is a finding, not a crash.
    let missing = repo.dir.join("missing").display().to_string();
    let report = repo.doctor_in(&repo.dir, &["--path", &missing], &[]);
    assert_eq!(report.code, Some(1));
    assert!(report
        .detail("repository.git")
        .contains("is not a directory"));
}

#[test]
fn a_store_from_another_version_is_reported_and_left_as_it_is() {
    let repo = Repo::used();
    let db = repo.state_db();
    assert_eq!(schema_of(&db), "2");

    // Older: doctor says opening will upgrade it, and does not open it.
    set_schema(&db, "1");
    let report = repo.doctor(&[]);
    assert_eq!(report.code, Some(0), "{}", report.stdout);
    assert_eq!(report.status("repository.schema"), "warn");
    assert!(report.detail("repository.schema").contains("schema 1"));
    assert!(report.detail("repository.schema").contains("reads 2"));
    assert_eq!(report.status("repository.state"), "warn");
    assert!(report.detail("repository.state").starts_with("not opened"));
    assert_eq!(schema_of(&db), "1", "doctor upgraded the store");

    // Newer: a failure, with what to do.
    set_schema(&db, "99");
    let report = repo.doctor(&[]);
    assert_eq!(report.code, Some(1));
    assert_eq!(report.status("repository.schema"), "fail");
    assert!(report.detail("repository.schema").contains("newer by"));
    assert!(report.fix("repository.schema").contains("newer by"));
    assert_eq!(schema_of(&db), "99");

    // Not a database at all: a failure naming the file.
    fs::write(&db, "this is not a database, and is longer than a header").unwrap();
    let report = repo.doctor(&[]);
    assert_eq!(report.status("repository.schema"), "fail");
    assert!(report.detail("repository.schema").contains("state.db"));
}

#[test]
fn credentials_are_named_and_their_values_never_shown() {
    let repo = Repo::used();
    let secret = "sk-ant-this-value-must-not-appear";
    let report = repo.doctor_in(&repo.root, &[], &[("ANTHROPIC_API_KEY", secret)]);
    assert_eq!(report.status("harnesses.credentials"), "ok");
    assert!(report
        .detail("harnesses.credentials")
        .contains("ANTHROPIC_API_KEY"));
    assert!(!report.stdout.contains(secret), "{}", report.stdout);
    let out = repo
        .command(repo.by_path())
        .env("ANTHROPIC_API_KEY", secret)
        .arg("doctor")
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&out.stdout).contains(secret));
    assert!(!String::from_utf8_lossy(&out.stderr).contains(secret));

    // The keys Gemini CLI, Antigravity and DeepSeek Harness read are among
    // the usual variables.
    let report = repo.doctor_in(
        &repo.root,
        &[],
        &[
            ("GEMINI_API_KEY", "g-never-shown"),
            ("DEEPSEEK_API_KEY", "d-never-shown"),
        ],
    );
    let detail = report.detail("harnesses.credentials");
    assert!(detail.contains("DEEPSEEK_API_KEY"), "{detail}");
    assert!(detail.contains("GEMINI_API_KEY"), "{detail}");
    assert!(!report.stdout.contains("never-shown"), "{}", report.stdout);

    // A declared secret with nothing behind it is named, with its source.
    repo.project_file("[secrets]\nDEPLOY_KEY = \"DOCTOR_TEST_DEPLOY_VAR\"\n");
    let report = repo.doctor(&[]);
    assert_eq!(report.status("harnesses.credentials"), "warn");
    assert!(report
        .detail("harnesses.credentials")
        .contains("DEPLOY_KEY (from $DOCTOR_TEST_DEPLOY_VAR)"));
    let report = repo.doctor_in(&repo.root, &[], &[("DOCTOR_TEST_DEPLOY_VAR", "v")]);
    assert_eq!(report.status("harnesses.credentials"), "ok");
}

#[test]
fn the_default_harness_must_be_known_and_installed() {
    let repo = Repo::used();
    // The loader refuses a name no profile has, and doctor shows why.
    repo.project_file("[defaults]\nharness = \"no-such-harness\"\n");
    let report = repo.doctor(&[]);
    assert_eq!(report.code, Some(1));
    assert_eq!(report.status("config.project"), "fail");
    assert!(report.detail("config.project").contains("defaults.harness"));

    // A harness Branchyard knows and this machine does not have.
    repo.project_file("[defaults]\nharness = \"deepseek-harness\"\n");
    let report = repo.doctor(&[]);
    assert_eq!(report.code, Some(1), "{}", report.stdout);
    assert_eq!(report.status("harnesses.default"), "fail");
    assert!(report
        .detail("harnesses.default")
        .contains("\"deepseek-harness\" is not installed"));
    assert!(report
        .fix("harnesses.default")
        .contains("by harnesses install deepseek-harness"));

    // Unset, `by run` starts claude-code, which must be there.
    repo.project_file("");
    let system = [("PATH", "/usr/bin:/bin"), ("BRANCHYARD_HARNESS_DIRS", "")];
    let report = repo.doctor_in(&repo.root, &[], &system);
    let claude_in_system_dirs =
        Path::new("/usr/bin/claude").exists() || Path::new("/bin/claude").exists();
    if !claude_in_system_dirs {
        assert_eq!(report.status("harnesses.default"), "warn");
        assert!(report
            .detail("harnesses.default")
            .contains("built-in default claude-code is not installed"));
        assert!(report.fix("harnesses.default").contains("--harness"));
    }
}

#[test]
fn the_provider_is_checked_as_the_engine_checks_it() {
    let repo = Repo::used();
    assert_eq!(repo.doctor(&[]).status("provider.default"), "ok");
    assert_eq!(repo.doctor(&[]).status("provider.sandboxes"), "ok");

    // The configuration's own checks come first: a provider with no
    // section to run it is refused when the file loads.
    repo.project_file("[defaults]\nprovider = \"microsandbox\"\n");
    let report = repo.doctor(&[]);
    assert_eq!(report.status("config.project"), "fail");
    assert!(report.detail("config.project").contains("[microsandbox]"));
    assert_eq!(report.status("provider.default"), "warn");

    repo.project_file(
        "[defaults]\nprovider = \"microsandbox\"\n[microsandbox]\nimage = \"alpine:3.20\"\n\
         max_kept_minutes = 30\n",
    );
    let report = repo.doctor(&[]);
    if cfg!(feature = "microsandbox") {
        assert_eq!(report.status("provider.default"), "ok", "{}", report.stdout);
    } else {
        assert_eq!(report.status("provider.default"), "fail");
        assert!(report
            .detail("provider.default")
            .contains("no Microsandbox support"));
        assert!(report
            .fix("provider.default")
            .contains("--features microsandbox"));
    }
    // No sandbox is kept yet; the limit is named once there are some.
    assert_eq!(report.status("provider.sandboxes"), "ok");
    assert!(report
        .detail("provider.sandboxes")
        .starts_with("0 kept sandboxes, 0 snapshots"));
    assert!(report
        .detail("provider.sandboxes")
        .contains("none over max_kept_minutes = 30"));

    repo.project_file("[defaults]\nprovider = \"recipe\"\nrecipe = \"box\"\n");
    let report = repo.doctor(&[]);
    assert_eq!(report.status("provider.default"), "fail");
    assert!(report
        .detail("provider.default")
        .contains("no [recipes.box]"));
    repo.project_file(
        "[defaults]\nprovider = \"recipe\"\nrecipe = \"box\"\n[recipes.box]\ncreate = \"true\"\n",
    );
    let report = repo.doctor(&[]);
    assert_eq!(report.status("provider.default"), "warn");
    assert!(report
        .fix("provider.default")
        .contains("by recipe trust box"));
    repo.ok(&["recipe", "trust", "box"]);
    assert_eq!(repo.doctor(&[]).status("provider.default"), "ok");
}

#[test]
fn confinement_that_must_hold_but_cannot_is_a_failure() {
    let repo = Repo::used();
    // Confinement turned off, as on a host that forbids user namespaces.
    let off = [("BRANCHYARD_EGRESS_NETNS", "off")];
    let report = repo.doctor_in(&repo.root, &[], &off);
    assert_eq!(report.status("host.namespaces"), "warn");
    assert!(report
        .detail("host.namespaces")
        .contains("BRANCHYARD_EGRESS_NETNS=off"));
    assert_eq!(report.code, Some(0));

    repo.project_file("[network]\nallow = [\"github.com\"]\nenforce = \"required\"\n");
    let report = repo.doctor_in(&repo.root, &[], &off);
    assert_eq!(report.code, Some(1), "{}", report.stdout);
    assert_eq!(report.status("host.namespaces"), "fail");
    assert!(report
        .detail("host.namespaces")
        .contains("enforce = \"required\""));
    // best_effort runs advisory, so it only warns.
    repo.project_file("[network]\nallow = [\"github.com\"]\n");
    assert_eq!(
        repo.doctor_in(&repo.root, &[], &off)
            .status("host.namespaces"),
        "warn"
    );
    // The loopback listener the proxy needs works wherever the tests do.
    assert_eq!(repo.doctor(&[]).status("host.loopback"), "ok");
}

#[test]
fn a_configured_server_is_asked_once_with_its_token() {
    let repo = Repo::used();
    let token = repo.dir.join("token");
    fs::write(&token, "tok-secret\n").unwrap();
    let token_arg = token.display().to_string();
    let mock = MockHttp::start(|request| {
        if request.path == "/v1/repos"
            && request.header("authorization") == Some("Bearer tok-secret")
        {
            Response::json(
                200,
                &json!({"repos": [{"name": "app", "root": "/srv/app"}]}),
            )
        } else {
            Response::json(
                401,
                &json!({"error": {"code": "unauthorized", "message": "the token is not accepted"}}),
            )
        }
    });
    let url = mock.url();
    let report = repo.doctor(&["--remote", &url, "--token-file", &token_arg]);
    assert_eq!(report.code, Some(0), "{}", report.stdout);
    assert_eq!(report.status("server.reachable"), "ok");
    assert!(report
        .detail("server.reachable")
        .contains("serves repository app"));
    assert!(!report.stdout.contains("tok-secret"));
    assert_eq!(mock.requests().len(), 1);

    // The repository the configuration names must be one it serves.
    let report = repo.doctor(&[
        "--remote",
        &url,
        "--token-file",
        &token_arg,
        "--repo",
        "other",
    ]);
    assert_eq!(report.status("server.reachable"), "fail");
    assert!(report.detail("server.reachable").contains("not \"other\""));

    // A token it refuses.
    fs::write(&token, "wrong\n").unwrap();
    let report = repo.doctor(&["--remote", &url, "--token-file", &token_arg]);
    assert_eq!(report.code, Some(1));
    assert_eq!(report.status("server.reachable"), "fail");
    assert!(report
        .detail("server.reachable")
        .contains("the token is not accepted"));

    // No token file at all.
    let report = repo.doctor(&["--remote", &url]);
    assert_eq!(report.status("server.reachable"), "fail");
    assert!(report.fix("server.reachable").contains("token_file"));

    // The same server named by the configuration.
    fs::write(&token, "tok-secret\n").unwrap();
    repo.project_file(&format!(
        "[remote]\nurl = \"{url}\"\ntoken_file = \"{token_arg}\"\nrepo = \"app\"\n"
    ));
    let report = repo.doctor(&[]);
    assert_eq!(report.status("server.reachable"), "ok", "{}", report.stdout);

    // Nothing listening: refused at once, not after a long wait.
    mock.finish();
    let report = repo.doctor(&[]);
    assert_eq!(report.status("server.reachable"), "fail");
    assert!(report
        .detail("server.reachable")
        .contains("did not answer GET /v1/repos"));

    // An ssh remote starts its own server; doctor does not.
    repo.project_file("");
    let report = repo.doctor(&["--remote", "ssh://host/srv/app"]);
    assert_eq!(report.status("server.reachable"), "ok");
    assert!(report
        .detail("server.reachable")
        .contains("not checked here"));
}

#[test]
fn the_model_gateways_keys_are_named_and_its_routed_models_priced() {
    let repo = Repo::used();
    repo.project_file(
        "[models.backends.anthropic]\napi = \"anthropic\"\nkey = \"DOCTOR_TEST_MODEL_KEY\"\n\
         [models.backends.openai]\napi = \"openai\"\nkey = \"DOCTOR_TEST_MODEL_KEY\"\n\
         [models.backends.local]\napi = \"generic\"\nurl = \"http://127.0.0.1:9\"\n\
         [[models.routes]]\nmodel = \"claude-sonnet-4-6\"\nbackends = [\"anthropic\"]\n\
         [[models.routes]]\nmodel = \"claude-opus-4-1\"\nbackends = [\"anthropic\"]\n\
         fallbacks = [\"openai\"]\nbackend_model = \"made-up-gpt\"\n\
         [[models.routes]]\nmodel = \"made-up-model\"\nbackends = [\"local\"]\n\
         [[models.routes]]\nmodel = \"gpt-*\"\nbackends = [\"local\"]\n",
    );
    let report = repo.doctor(&[]);
    assert_eq!(report.code, Some(1), "{}", report.stdout);
    assert_eq!(report.status("models.keys"), "fail");
    let keys = report.detail("models.keys");
    assert!(
        keys.contains("anthropic key $DOCTOR_TEST_MODEL_KEY is NOT set"),
        "{keys}"
    );
    assert!(keys.contains("local takes no key"), "{keys}");
    assert!(report.fix("models.keys").contains("export the variable"));
    assert_eq!(report.status("models.prices"), "warn");
    let prices = report.detail("models.prices");
    // The unpriced models, in route order: the opus route's fallback is
    // asked for a model nothing prices and is named for it, while the
    // catalog prices the route itself on its Anthropic backend.
    assert!(
        prices.contains(
            "no price for claude-opus-4-1 via openai as made-up-gpt, made-up-model: its calls"
        ),
        "{prices}"
    );
    assert!(
        !prices.contains("claude-sonnet-4-6"),
        "the catalog prices it: {prices}"
    );
    assert_eq!(prices.matches("claude-opus-4-1").count(), 1, "{prices}");
    assert!(
        prices.contains("gpt-*"),
        "a pattern is named as not checked: {prices}"
    );

    let secret = "sk-model-key-value-must-not-appear";
    let report = repo.doctor_in(&repo.root, &[], &[("DOCTOR_TEST_MODEL_KEY", secret)]);
    assert_eq!(report.status("models.keys"), "ok", "{}", report.stdout);
    assert!(!report.stdout.contains(secret));

    // A price in the configuration covers the model.
    repo.project_file(
        "[models.backends.local]\napi = \"generic\"\nurl = \"http://127.0.0.1:9\"\n\
         [models.backends.openai]\napi = \"openai\"\n\
         [[models.routes]]\nmodel = \"made-up-model\"\nbackends = [\"local\"]\n\
         [[models.routes]]\nmodel = \"claude-opus-4-1\"\nbackends = [\"openai\"]\n\
         backend_model = \"made-up-gpt\"\n\
         [models.prices.\"made-up-*\"]\ninput = 1.0\noutput = 2.0\n",
    );
    let report = repo.doctor(&[]);
    assert_eq!(report.status("models.prices"), "ok", "{}", report.stdout);
}
