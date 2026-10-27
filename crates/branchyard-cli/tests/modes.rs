//! Where harnesses run, said truthfully (docs/modes.md): one banner for each
//! mode, the mode recorded on the branch and shown by `show`, `inspect` and
//! `ls --json`, `--isolated=false` overriding `[defaults] isolated = true`,
//! the refusals of a build without a feature naming the command that adds
//! it, a server configuration this build cannot serve failing `by serve
//! --check` and `by doctor`, and a remote that does not answer ending with
//! what to do. Hermetic: the fake ACP agent, no network (a port nothing
//! listens on is refused at once), no database. Requires `git` and `sh`.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
use std::fs;
use std::process::Output;

use serde_json::Value;

fn repo() -> branchyard_testkit::Repo {
    branchyard_testkit::repo!()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The line of stderr that begins `by: `, which says where the run happens.
fn banner(out: &Output) -> String {
    stderr(out)
        .lines()
        .find(|l| l.contains(" mode"))
        .unwrap_or_default()
        .to_owned()
}

#[test]
fn each_way_of_running_says_so_and_the_record_agrees() {
    let repo = repo();

    let plain = repo.by_agent(&["run", "WRITE a.txt=1", "--name", "plain"]);
    assert!(plain.status.success(), "{}", stderr(&plain));
    let line = banner(&plain);
    assert!(line.starts_with("by: local mode:"), "{line}");
    assert!(line.contains("no sandbox"), "{line}");
    assert!(
        !stderr(&plain).contains("isolated mode"),
        "{}",
        stderr(&plain)
    );

    let isolated = repo.by_agent(&["run", "WRITE b.txt=1", "--name", "private", "--isolated"]);
    assert!(isolated.status.success(), "{}", stderr(&isolated));
    let line = banner(&isolated);
    assert!(line.starts_with("by: isolated mode:"), "{line}");
    assert!(line.contains("private HOME"), "{line}");
    assert!(line.contains("not a sandbox"), "{line}");
    assert!(
        repo.root.join(".branchyard/homes/private").is_dir(),
        "the banner describes what happened"
    );

    // `by ls --json`, `by show --json` and `by inspect --json` carry it.
    let list = repo.json(&["ls", "--json"]);
    let by_name = |name: &str| -> Value {
        list.as_array()
            .unwrap()
            .iter()
            .find(|b| b["name"] == name)
            .unwrap()
            .clone()
    };
    assert_eq!(by_name("plain")["provider"], "local");
    assert_eq!(by_name("plain")["isolated"], false);
    assert_eq!(by_name("private")["provider"], "local");
    assert_eq!(by_name("private")["isolated"], true);
    let shown = repo.json(&["show", "private", "--json"]);
    assert_eq!(shown["provider"], "local");
    assert_eq!(shown["isolated"], true);
    let inspected = repo.json(&["inspect", "private", "--json"]);
    assert_eq!(inspected["provider"], "local");
    assert_eq!(inspected["isolated"], true);

    // And the text commands say it in words.
    let show = stdout(&repo.ok(&["show", "private"]));
    assert!(
        show.contains("local, isolated: private HOME, not a sandbox"),
        "{show}"
    );
    let show = stdout(&repo.ok(&["show", "plain"]));
    assert!(show.contains("local: as your user, no sandbox"), "{show}");
    let inspect = stdout(&repo.ok(&["inspect", "private"]));
    assert!(
        inspect.contains("local, isolated: private HOME, not a sandbox"),
        "{inspect}"
    );

    // A send to the isolated branch says what it is, though the flags don't.
    let sent = repo.by_agent(&["send", "private", "WRITE c.txt=1"]);
    assert!(sent.status.success(), "{}", stderr(&sent));
    assert!(
        banner(&sent).starts_with("by: isolated mode:"),
        "{}",
        banner(&sent)
    );
}

/// A run refused before it starts leaves the repository as it found it:
/// no `.branchyard/` (round 8: a refused run left `state.db`,
/// `worktrees/` and `gateway/`). And the local-mode note, said until a run
/// it was printed for has started its harness, is not spent by a command
/// that failed first (round 8: a `--harness` typo printed it, and the first
/// real run said nothing).
#[test]
fn a_refused_run_leaves_no_yard_and_keeps_the_local_note_for_a_real_one() {
    let repo = repo();
    let yard = repo.root.join(".branchyard");
    let typo = repo.by_agent(&["run", "WRITE a.txt=1", "--harness", "nonesuch"]);
    assert!(!typo.status.success());
    assert!(
        stderr(&typo).contains("no harness or profile named nonesuch"),
        "{}",
        stderr(&typo)
    );
    assert!(!yard.exists(), "an unknown harness created .branchyard/");
    let unapproved = repo.by_agent(&["run", "WRITE a.txt=1", "--harness", "pi"]);
    assert!(!unapproved.status.success());
    assert!(
        stderr(&unapproved).contains("does not route tool permission requests"),
        "{}",
        stderr(&unapproved)
    );
    let fan = repo.by_agent(&["fan", "WRITE a.txt=1", "--harness", "gemini-cli,nonesuch"]);
    assert!(!fan.status.success());
    assert!(!yard.exists(), "a refused run or fan created .branchyard/");

    // Past those, a base that does not exist fails before the harness
    // starts: the note is printed, and not spent.
    let based = repo.by_agent(&["run", "WRITE a.txt=1", "--base", "nonesuch"]);
    assert!(!based.status.success());
    assert!(
        banner(&based).starts_with("by: local mode:"),
        "{}",
        stderr(&based)
    );
    assert!(!yard.join("local-mode-noted").exists());
    let first = repo.by_agent(&["run", "WRITE a.txt=1", "--name", "first"]);
    assert!(first.status.success(), "{}", stderr(&first));
    assert!(
        banner(&first).starts_with("by: local mode:"),
        "{}",
        stderr(&first)
    );
    assert!(yard.join("local-mode-noted").is_file());
    let second = repo.by_agent(&["run", "WRITE b.txt=1", "--name", "second"]);
    assert!(second.status.success(), "{}", stderr(&second));
    assert!(banner(&second).is_empty(), "{}", stderr(&second));
}

#[test]
fn isolated_false_turns_off_what_the_project_file_turned_on() {
    let repo = repo();
    fs::write(
        repo.root.join("branchyard.toml"),
        "version = 1\n[defaults]\nisolated = true\n",
    )
    .unwrap();

    let configured = repo.by_agent(&["run", "WRITE a.txt=1", "--name", "configured"]);
    assert!(configured.status.success(), "{}", stderr(&configured));
    assert!(
        banner(&configured).starts_with("by: isolated mode:"),
        "{}",
        banner(&configured)
    );

    let shared = repo.by_agent(&[
        "run",
        "WRITE b.txt=1",
        "--name",
        "shared",
        "--isolated=false",
    ]);
    assert!(shared.status.success(), "{}", stderr(&shared));
    assert!(
        banner(&shared).starts_with("by: local mode:"),
        "{}",
        banner(&shared)
    );
    assert!(!repo.root.join(".branchyard/homes/shared").exists());
    assert_eq!(repo.json(&["show", "shared", "--json"])["isolated"], false);
    assert_eq!(
        repo.json(&["show", "configured", "--json"])["isolated"],
        true
    );

    // The bare flag still means yes, and so do the other spellings of it.
    for (name, flag) in [("bare", "--isolated"), ("yes", "--isolated=yes")] {
        let out = repo.by_agent(&["run", "WRITE c.txt=1", "--name", name, flag]);
        assert!(out.status.success(), "{flag}: {}", stderr(&out));
        assert_eq!(
            repo.json(&["show", name, "--json"])["isolated"],
            true,
            "{flag}"
        );
    }
    // Not a value: refused as a usage error, not run as an unnamed prompt.
    let bad = repo.by_agent(&["run", "WRITE d.txt=1", "--isolated=maybe"]);
    assert_eq!(bad.status.code(), Some(2), "{}", stderr(&bad));
}

#[test]
fn help_says_a_private_home_is_not_a_sandbox() {
    let repo = repo();
    let run = stdout(&repo.ok(&["run", "--help"]));
    let at = run
        .find("--isolated")
        .unwrap_or_else(|| panic!("--isolated in by run --help"));
    let about = &run[at..run[at..].find("--command").map_or(run.len(), |n| at + n)];
    assert!(about.contains("not a sandbox"), "{about}");
    assert!(about.contains("--isolated=false"), "{about}");
    let general = stdout(&repo.ok(&["--help"]));
    assert!(general.contains("docs/modes.md"), "{general}");
    assert!(general.contains("not a sandbox"), "{general}");
}

/// A build without the Microsandbox SDK refuses `--provider microsandbox`
/// before it creates a branch, and the refusal says what to run.
#[cfg(not(feature = "microsandbox"))]
#[test]
fn a_build_without_microsandbox_names_the_command_that_adds_it() {
    let repo = repo();
    let out = repo.by_agent(&[
        "run",
        "WRITE a.txt=1",
        "--name",
        "boxed",
        "--provider",
        "microsandbox",
        "--image",
        "alpine:3.20",
    ]);
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    let text = stderr(&out);
    assert!(text.contains("no Microsandbox support"), "{text}");
    assert!(
        text.contains(
            "cargo install --locked --path crates/branchyard-cli --features microsandbox"
        ),
        "{text}"
    );
    assert!(text.contains("KVM") && text.contains("libcap-ng"), "{text}");
    assert_eq!(
        repo.json(&["ls", "--json"]),
        Value::Array(Vec::new()),
        "no branch was created"
    );
}

/// A server configuration naming a database, in a repository.
fn server_file(repo: &branchyard_testkit::Repo, database: Option<&str>) -> String {
    let mut config = serde_json::json!({
        "listen": "127.0.0.1:0",
        "data_dir": "server",
        "repos": { "app": "repo" },
    });
    if let Some(url) = database {
        config["database"] = url.into();
    }
    let path = repo.dir.join("server.json");
    fs::write(&path, config.to_string()).unwrap();
    path.display().to_string()
}

/// `by serve --check` fails for a PostgreSQL configuration a build without
/// the feature cannot serve, as serving would; and passes it, without
/// connecting to anything, on a build that can.
#[test]
fn serve_check_fails_where_serving_would() {
    let repo = repo();
    // Nothing listens here: a pass proves `--check` did not connect.
    let path = server_file(&repo, Some("postgres://branchyard@127.0.0.1:1/none"));
    let check = repo.by(&["serve", "--config", &path, "--check"]);
    if cfg!(feature = "postgres") {
        assert!(check.status.success(), "{}", stderr(&check));
        assert!(stdout(&check).contains("configuration ok"));
    } else {
        assert_eq!(check.status.code(), Some(1), "{}", stderr(&check));
        let text = stderr(&check);
        assert!(text.contains("no PostgreSQL support"), "{text}");
        assert!(
            text.contains(
                "cargo install --locked --path crates/branchyard-cli --features postgres"
            ),
            "{text}"
        );
        assert!(!stdout(&check).contains("configuration ok"));
    }

    // SQLite needs no feature: it passes anywhere.
    let path = server_file(&repo, None);
    let check = repo.by(&["serve", "--config", &path, "--check"]);
    assert!(check.status.success(), "{}", stderr(&check));
    assert!(stdout(&check).contains("configuration ok"));
}

/// `by doctor` reads the server configuration `[serve] config` names, and
/// says which store it names; with no `[serve]` it has no such rows.
#[test]
fn doctor_reads_the_server_this_repository_would_run() {
    let repo = repo();
    repo.ok(&["ls"]);
    let rows = |repo: &branchyard_testkit::Repo| -> Value {
        let out = repo.by(&["doctor", "--json"]);
        serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("doctor printed JSON ({e}): {}", stdout(&out)))
    };
    let row = |report: &Value, name: &str| -> Option<Value> {
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .cloned()
    };

    let report = rows(&repo);
    assert!(row(&report, "serve.config").is_none());
    assert!(row(&report, "serve.database").is_none());

    // SQLite: both rows ok, the store named.
    let path = server_file(&repo, None);
    fs::write(
        repo.root.join("branchyard.toml"),
        format!("version = 1\n[serve]\nconfig = {path:?}\n"),
    )
    .unwrap();
    let report = rows(&repo);
    assert_eq!(row(&report, "serve.config").unwrap()["status"], "ok");
    let database = row(&report, "serve.database").unwrap();
    assert_eq!(database["status"], "ok");
    assert!(
        database["detail"]
            .as_str()
            .unwrap()
            .starts_with("SQLite in "),
        "{database}"
    );

    // PostgreSQL: named, never connected to; refused where the build cannot.
    let path = server_file(&repo, Some("postgres://by:hunter2@127.0.0.1:1/branchyard"));
    fs::write(
        repo.root.join("branchyard.toml"),
        format!("version = 1\n[serve]\nconfig = {path:?}\n"),
    )
    .unwrap();
    let report = rows(&repo);
    let config = row(&report, "serve.config").unwrap();
    let database = row(&report, "serve.database").unwrap();
    let detail = database["detail"].as_str().unwrap();
    assert!(
        detail.contains("PostgreSQL at 127.0.0.1:1/branchyard"),
        "{detail}"
    );
    assert!(
        !detail.contains("hunter2"),
        "a password is never shown: {detail}"
    );
    if cfg!(feature = "postgres") {
        // Not a failure; a password in the file earns the loader's chmod warning.
        assert_ne!(config["status"], "fail", "{config}");
        assert_eq!(database["status"], "ok", "{database}");
    } else {
        assert_eq!(config["status"], "fail", "{config}");
        assert!(
            config["detail"]
                .as_str()
                .unwrap()
                .contains("no PostgreSQL support"),
            "{config}"
        );
        assert_eq!(database["status"], "fail", "{database}");
        assert!(
            database["fix"]
                .as_str()
                .unwrap()
                .contains("--features postgres"),
            "{database}"
        );
    }

    // A file that does not load is a failed row, and the second row says it
    // could not look.
    fs::write(&path, "{ not json").unwrap();
    let report = rows(&repo);
    assert_eq!(row(&report, "serve.config").unwrap()["status"], "fail");
    let database = row(&report, "serve.database").unwrap();
    assert_eq!(database["status"], "warn");
    assert!(
        database["detail"]
            .as_str()
            .unwrap()
            .starts_with("not checked: "),
        "{database}"
    );
}

/// The ways in which a remote command with nothing to talk to ends: the
/// token's three homes, and what to do about a server that does not answer.
#[test]
fn a_remote_dead_end_ends_with_the_fix() {
    let mut repo = repo();
    repo.remove_env("BRANCHYARD_REMOTE");
    repo.remove_env("BRANCHYARD_TOKEN_FILE");

    // No token anywhere: the flag, the variable and the file key.
    let out = repo.by(&["--remote", "http://127.0.0.1:9", "ls"]);
    assert_eq!(out.status.code(), Some(2));
    let text = stderr(&out);
    assert!(
        text.starts_with("by: remote mode needs a token\n  next: "),
        "{text}"
    );
    assert!(text.contains("--token-file"), "{text}");
    assert!(text.contains("BRANCHYARD_TOKEN_FILE"), "{text}");
    assert!(text.contains("token_file under [remote]"), "{text}");
    let json = repo.by(&["--remote", "http://127.0.0.1:9", "ls", "--json"]);
    assert_eq!(json.status.code(), Some(2));
    let envelope: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(envelope["error"]["kind"], "usage", "{envelope}");
    assert_eq!(
        envelope["error"]["next"].as_array().map(Vec::len),
        Some(2),
        "{envelope}"
    );

    // A token and no server: the reason, then the way out.
    let token = repo.dir.join("token");
    fs::write(&token, "0123456789abcdef0123456789abcdef\n").unwrap();
    let token = token.display().to_string();
    let out = repo.by(&[
        "--remote",
        "http://127.0.0.1:9",
        "--token-file",
        &token,
        "--repo",
        "app",
        "ls",
    ]);
    assert_eq!(out.status.code(), Some(3));
    let text = stderr(&out);
    assert!(
        text.starts_with("by: cannot reach http://127.0.0.1:9"),
        "{text}"
    );
    assert!(
        text.contains(
            "next: start a server with `by serve` on that host, or fix the URL, token or CA file"
        ),
        "{text}"
    );

    // The same through `[remote]` in the project file.
    fs::write(
        repo.root.join("branchyard.toml"),
        format!("version = 1\n[remote]\nurl = \"http://127.0.0.1:9\"\ntoken_file = {token:?}\nrepo = \"app\"\n"),
    )
    .unwrap();
    let out = repo.by(&["ls"]);
    assert_eq!(out.status.code(), Some(3));
    assert!(
        stderr(&out).contains("next: start a server with `by serve`"),
        "{}",
        stderr(&out)
    );

    // And doctor's row for the same case says the same words.
    let report = repo.by(&["doctor", "--json"]);
    let report: Value = serde_json::from_slice(&report.stdout).unwrap();
    let row = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "server.reachable")
        .unwrap();
    assert!(
        row["fix"]
            .as_str()
            .unwrap()
            .starts_with("start a server with `by serve` on that host"),
        "{row}"
    );
}

/// `provider = "recipe:NAME"` is the flag's spelling; the file refuses it
/// with the spelling it takes. The microsandbox section takes the flags'
/// names as well as its own.
#[test]
fn the_project_file_points_from_the_flags_spelling_to_its_own() {
    let repo = repo();
    fs::write(
        repo.root.join("branchyard.toml"),
        "version = 1\n[defaults]\nprovider = \"recipe:devbox\"\n",
    )
    .unwrap();
    let out = repo.by(&["config", "validate"]);
    assert!(!out.status.success(), "{}", stdout(&out));
    let text = format!("{}{}", stdout(&out), stderr(&out));
    assert!(text.contains("provider = \"recipe\""), "{text}");
    assert!(text.contains("recipe = \"devbox\""), "{text}");
    assert!(text.contains("line 3"), "{text}");

    fs::write(
        repo.root.join("branchyard.toml"),
        "version = 1\n[defaults]\nprovider = \"substrate\"\n",
    )
    .unwrap();
    let out = repo.by(&["config", "validate"]);
    let text = format!("{}{}", stdout(&out), stderr(&out));
    assert!(text.contains("--provider substrate"), "{text}");

    // The flags' names work as keys, and mean the same.
    fs::write(
        repo.root.join("branchyard.toml"),
        "version = 1\n[defaults]\nprovider = \"microsandbox\"\n[microsandbox]\n\
         image = \"alpine:3.20\"\nmemory = 2048\nkeep_sandbox = \"destroy\"\n\
         sandbox_snapshots = 2\n",
    )
    .unwrap();
    let shown = repo.json(&["config", "show", "--json"]);
    let values = &shown["values"];
    assert_eq!(values["microsandbox.memory_mib"]["value"], 2048);
    assert_eq!(values["microsandbox.keep"]["value"], "destroy");
    assert_eq!(values["microsandbox.snapshots"]["value"], 2);
    assert!(repo.by(&["config", "validate"]).status.success());
}
