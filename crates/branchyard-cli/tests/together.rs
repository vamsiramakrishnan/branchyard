//! Where round 9's changes meet, through the built `by`: a gateway
//! provider login (`by harnesses login ID --provider`) carried into an
//! isolated run that the file guard holds (docs/harness-lifecycle.md,
//! docs/modes.md#the-file-guard); and each refusal before a run exiting
//! with its own status (docs/cli.md#exit-status) without leaving a
//! `.branchyard/` behind. Hermetic: the fake agent stands in for the
//! harness, and no model is called.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::Value;

fn repo() -> branchyard_testkit::Repo {
    branchyard_testkit::repo!()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// An executable script `name` in `dir`.
fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// The program `name` on this test's own `PATH`.
fn which(name: &str) -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("{name} is not on PATH"))
}

/// A `bin` directory beside the repository holding only git and sh, so no
/// harness the machine has installed is found.
fn bare_path(repo: &branchyard_testkit::Repo) -> PathBuf {
    let bin = repo.dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    for tool in ["git", "sh"] {
        std::os::unix::fs::symlink(which(tool), bin.join(tool)).unwrap();
    }
    bin
}

/// `--json`'s envelope on stdout.
fn envelope(out: &std::process::Output) -> Value {
    let value: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "not JSON ({e}):\n{}\n{}",
            text(&out.stdout),
            text(&out.stderr)
        )
    });
    value["error"].clone()
}

/// `by harnesses login qwen-code --provider zai` stores the key; an isolated
/// run under `--permissions edit-worktree` gets it under Qwen Code's own
/// variable in its private home, and the guard still holds the harness to
/// its worktree: the write inside lands, the one beside the worktree does
/// not, and the record says both how it ran and what held it.
#[test]
fn a_provider_login_runs_isolated_inside_the_file_guard() {
    if let Err(why) = branchyard::guard::support() {
        eprintln!("SKIPPED a_provider_login_runs_isolated_inside_the_file_guard: {why}");
        return;
    }
    let mut repo = repo();
    let home = repo.dir.join("home");
    fs::create_dir_all(&home).unwrap();
    let bin = bare_path(&repo);
    let agent = branchyard_testkit::fake_agent(repo.by_path())
        .display()
        .to_string();
    script(
        &bin,
        "qwen",
        &format!(
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 0.26.0; exit 0; fi\nexec {agent}\n"
        ),
    );
    repo.set_env("HOME", home.to_str().unwrap());
    repo.set_env("PATH", &format!("{}:/usr/bin:/bin", bin.display()));
    let key = "zai-test-0123456789abcdef";

    let login = repo.by_with_stdin(
        &["harnesses", "login", "qwen-code", "--provider", "zai"],
        key,
    );
    assert!(login.status.success(), "{}", text(&login.stderr));
    assert!(!text(&login.stdout).contains(key) && !text(&login.stderr).contains(key));

    let run = repo.by(&[
        "run",
        "WRITE inside.txt=in WRITE ../outside.txt=out",
        "--harness",
        "qwen-code",
        "--isolated",
        "--permissions",
        "edit-worktree",
        "-n",
        "glm",
    ]);
    let stderr = text(&run.stderr);
    assert!(run.status.success(), "{stderr}");
    let guard = stderr
        .lines()
        .find(|l| l.starts_with("by: guard:"))
        .unwrap_or_else(|| panic!("no guard line:\n{stderr}"));
    assert!(
        guard.contains("landlock (edit-worktree)") && guard.contains("private HOME"),
        "{guard}"
    );

    // The key reached the harness under its own variable, and only there.
    // A send keeps the branch's private home; its permissions are its own
    // flags, as a run's are, so the same preset is guarded the same way.
    let send = repo.by(&[
        "send",
        "glm",
        "ENV OPENAI_API_KEY OPENAI_BASE_URL ZAI_API_KEY",
        "--permissions",
        "edit-worktree",
    ]);
    assert!(send.status.success(), "{}", text(&send.stderr));
    assert!(
        text(&send.stderr).contains("by: guard: landlock (edit-worktree)"),
        "{}",
        text(&send.stderr)
    );
    let log = text(&repo.ok(&["log", "glm"]).stdout);
    assert!(
        log.contains("write ../outside.txt failed: Permission denied"),
        "{log}"
    );
    for line in [
        format!("OPENAI_API_KEY={key}"),
        "OPENAI_BASE_URL=https://api.z.ai/api/coding/paas/v4".to_owned(),
        "ZAI_API_KEY unset".to_owned(),
    ] {
        assert!(log.contains(&line), "{line}\n{log}");
    }

    // The guard held: the write inside the worktree is the candidate, the
    // one beside it never happened.
    let worktrees = repo.root.join(".branchyard/worktrees");
    assert!(
        !worktrees.join("outside.txt").exists(),
        "a write beside the worktree landed"
    );
    let diff = text(&repo.ok(&["diff", "glm"]).stdout);
    assert!(diff.contains("inside.txt"), "{diff}");

    // The record says it ran isolated and what held it; the person's own
    // HOME is not in the scope, the private one is.
    let show = repo.json(&["show", "glm", "--json"]);
    assert_eq!(show["isolated"], true, "{show}");
    assert_eq!(show["guard"]["kind"], "landlock", "{show}");
    let writable: Vec<&str> = show["guard"]["writable"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    assert!(
        writable.iter().all(|p| !Path::new(p).starts_with(&home)),
        "{writable:?}"
    );
    assert!(
        writable
            .iter()
            .any(|p| p.ends_with("glm") && p.contains("worktrees")),
        "{writable:?}"
    );
}

/// Every refusal that comes before a run says so in its own exit status,
/// as the envelope's `exit` does, and none of them leaves `.branchyard/`.
#[test]
fn each_refusal_before_a_run_has_its_status_and_leaves_no_yard() {
    let mut repo = repo();
    let yard = repo.root.join(".branchyard");
    // git and sh, then the system's own directories (the detection script
    // needs its tools), and a HOME of its own: no harness is installed.
    let bin = bare_path(&repo);
    let home = repo.dir.join("home");
    fs::create_dir_all(&home).unwrap();
    repo.set_env("PATH", &format!("{}:/usr/bin:/bin", bin.display()));
    repo.set_env("HOME", home.to_str().unwrap());

    // A harness nobody knows: a usage error.
    let typo = repo.by_agent(&["run", "WRITE a.txt=1", "--harness", "nonesuch", "--json"]);
    let error = envelope(&typo);
    assert_eq!(typo.status.code(), Some(2), "{error}");
    assert_eq!(error["kind"], "unknown_harness", "{error}");
    assert_eq!(error["exit"], 2, "{error}");

    // A profile whose tools would bypass the policy: refused.
    let pi = repo.by_agent(&["run", "WRITE a.txt=1", "--harness", "pi", "--json"]);
    let error = envelope(&pi);
    assert_eq!(pi.status.code(), Some(4), "{error}");
    assert_eq!(error["kind"], "unsupported", "{error}");
    assert_eq!(
        error["detail"]["unsupported"], "unrouted_approvals",
        "{error}"
    );
    assert_eq!(error["exit"], 4, "{error}");

    // A harness this machine does not have: the environment.
    let missing = repo.by(&["run", "WRITE a.txt=1", "--harness", "codex", "--json"]);
    let error = envelope(&missing);
    assert_eq!(missing.status.code(), Some(3), "{error}");
    assert_eq!(error["kind"], "harness_unavailable", "{error}");
    assert_eq!(error["exit"], 3, "{error}");
    // Installs are off by default: the next step is the catalog's own
    // command, and the message says how to let `by` run it.
    let next = error["next"].as_array().unwrap();
    assert!(
        next.iter().any(|n| n.as_str().unwrap().contains("install")),
        "{error}"
    );
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("by harnesses install codex"),
        "{error}"
    );

    assert!(!yard.exists(), "a refused run created .branchyard/");
}
