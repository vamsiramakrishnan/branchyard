//! `by recipe` end to end: trust gating like `[workspace]`'s, and `check`
//! (the doctor, then create, exec over ssh, suspend, resume and destroy)
//! against a recipe whose scripts make a local "VM" reached through
//! `crates/branchyard-recipe/tests/fixtures/fake-ssh`. No cloud, no sshd.
//! Requires `git`, `sh`, `python3`, `ps`.

#![allow(clippy::let_underscore_must_use, clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const BY: &str = env!("CARGO_BIN_EXE_by");
const FAKE_SSH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../branchyard-recipe/tests/fixtures/fake-ssh"
);

const RECIPE: &str = r#"
[recipes.devbox]
description = "a local test VM"
create = "./vm/create.sh"
suspend = "./vm/suspend.sh"
resume = "./vm/resume.sh"
destroy = "./vm/destroy.sh"
doctor = "./vm/doctor.sh"
"#;

const SCRIPTS: &[(&str, &str)] = &[
    (
        "create.sh",
        r#"#!/bin/sh
set -e
mkdir -p "$BRANCHYARD_ROOT/.vms/$BRANCHYARD_RECIPE_INSTANCE" "$BRANCHYARD_ROOT/.vms/work"
printf '{"schemaVersion":1,"connection":{"type":"ssh","target":{"host":"vm.test","username":"dev"},"projectRoot":"%s"}}\n' "$BRANCHYARD_ROOT/.vms/work"
"#,
    ),
    ("suspend.sh", "#!/bin/sh\ncat >/dev/null\n"),
    (
        "resume.sh",
        r#"#!/bin/sh
cat >/dev/null
printf '{"schemaVersion":1,"connection":{"type":"ssh","target":{"host":"vm.test","username":"dev"},"projectRoot":"%s"}}\n' "$BRANCHYARD_ROOT/.vms/work"
"#,
    ),
    (
        "destroy.sh",
        "#!/bin/sh\ncat >\"$BRANCHYARD_ROOT/.vms/$BRANCHYARD_RECIPE_INSTANCE.destroyed\"\n",
    ),
    ("doctor.sh", "#!/bin/sh\necho \"vm-cli 1.2 ready\"\n"),
];

struct Repo {
    _dir: tempfile::TempDir,
    root: PathBuf,
    trust: PathBuf,
    user: PathBuf,
    ssh: PathBuf,
}

impl Repo {
    fn new() -> Repo {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("repo");
        fs::create_dir_all(root.join("vm")).unwrap();
        fs::create_dir_all(base.join("vm-home")).unwrap();
        assert!(Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&root)
            .status()
            .unwrap()
            .success());
        fs::write(root.join("branchyard.toml"), RECIPE).unwrap();
        for (name, text) in SCRIPTS {
            let path = root.join("vm").join(name);
            fs::write(&path, text).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let ssh = base.join("ssh");
        fs::write(
            &ssh,
            format!(
                "#!/bin/sh\nFAKE_SSH_HOME='{}' FAKE_SSH_HOSTS=vm.test exec python3 '{FAKE_SSH}' \
                 \"$@\"\n",
                base.join("vm-home").display()
            ),
        )
        .unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
        Repo {
            trust: base.join("trusted.json"),
            user: base.join("user.toml"),
            _dir: dir,
            root,
            ssh,
        }
    }

    fn by(&self, args: &[&str]) -> Output {
        Command::new(BY)
            .current_dir(&self.root)
            .args(args)
            .env("BRANCHYARD_TRUST_FILE", &self.trust)
            .env("BRANCHYARD_USER_CONFIG", &self.user)
            .env("BRANCHYARD_SSH", &self.ssh)
            .env("NO_COLOR", "1")
            .env_remove("BRANCHYARD_BRANCH")
            .env_remove("BRANCHYARD_REMOTE")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn vms(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(root.join(".vms"))
        .map(|d| {
            d.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

#[test]
fn recipes_run_only_once_trusted_and_check_runs_the_whole_lifecycle() {
    let repo = Repo::new();
    let out = repo.by(&["recipe", "list"]);
    assert!(
        text(&out.stdout).contains("devbox"),
        "{}",
        text(&out.stdout)
    );
    assert!(
        text(&out.stdout).contains("untrusted"),
        "{}",
        text(&out.stdout)
    );
    assert!(
        text(&out.stdout).contains("exec, pause"),
        "{}",
        text(&out.stdout)
    );

    // Without a terminal an untrusted recipe is refused, and nothing ran.
    let out = repo.by(&["recipe", "check", "devbox"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("by recipe trust devbox"),
        "{}",
        text(&out.stderr)
    );
    assert!(vms(&repo.root).is_empty());

    // A harness on a branch can never trust one.
    let out = Command::new(BY)
        .current_dir(&repo.root)
        .args(["recipe", "trust", "devbox"])
        .env("BRANCHYARD_TRUST_FILE", &repo.trust)
        .env("BRANCHYARD_BRANCH", "some-branch")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!repo.trust.exists());

    let out = repo.by(&["recipe", "trust", "devbox"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        fs::metadata(&repo.trust).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let out = repo.by(&["recipe", "show", "devbox", "--json"]);
    let shown: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(shown["trust"], "trusted");
    assert_eq!(shown["origin"], "project");

    let out = repo.by(&["recipe", "check", "devbox"]);
    let said = text(&out.stdout);
    assert!(out.status.success(), "{said}\n{}", text(&out.stderr));
    for line in [
        "pass  recipe.create",
        "pass  recipe.doctor.run",
        "the doctor command passed: vm-cli 1.2 ready",
        "pass  smoke.create",
        "reached by ssh dev@vm.test:22",
        "pass  smoke.exec ",
        "pass  smoke.suspend_resume",
        "pass  smoke.exec_after_resume",
        "pass  smoke.destroy",
        "recipe ok",
    ] {
        assert!(said.contains(line), "{line:?} in\n{said}");
    }
    assert!(
        said.contains(&format!(
            "ran in {}/.vms/work on Linux",
            repo.root.display()
        )),
        "{said}"
    );
    let left = vms(&repo.root);
    assert!(left.iter().any(|n| n.ends_with(".destroyed")), "{left:?}");

    let out = repo.by(&["recipe", "check", "devbox", "--no-smoke", "--json"]);
    let checked: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(checked["ok"], true);
    assert!(checked["checks"]
        .as_array()
        .unwrap()
        .iter()
        .all(|c| !c["id"].as_str().unwrap().starts_with("smoke")));

    // A change to any command asks again.
    let changed = RECIPE.replace("./vm/doctor.sh", "./vm/doctor.sh --verbose");
    fs::write(repo.root.join("branchyard.toml"), changed).unwrap();
    let out = repo.by(&["recipe", "check", "devbox"]);
    assert!(
        text(&out.stderr).contains("changed since you trusted it"),
        "{}",
        text(&out.stderr)
    );
    let out = repo.by(&["recipe", "untrust", "devbox"]);
    assert!(text(&out.stdout).contains("no longer trusted"));
}

#[test]
fn a_failing_doctor_or_create_fails_the_check_and_your_own_recipes_need_no_trust() {
    let repo = Repo::new();
    fs::write(
        &repo.user,
        format!(
            "[recipes.mine]\ncreate = \"{}/vm/create.sh\"\ndestroy = \"none\"\n\
             [recipes.broken]\ncreate = \"./vm/create.sh\"\ndoctor = \"echo vm-cli missing >&2; exit 4\"\n",
            repo.root.display()
        ),
    )
    .unwrap();
    let out = repo.by(&["recipe", "list", "--json"]);
    let listed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let trust = |name: &str| {
        listed
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == name)
            .map(|r| r["trust"].clone())
    };
    assert_eq!(trust("mine"), Some("not_needed".into()));
    assert_eq!(trust("devbox"), Some("untrusted".into()));

    let out = repo.by(&["recipe", "check", "mine"]);
    let said = text(&out.stdout);
    assert!(out.status.success(), "{said}\n{}", text(&out.stderr));
    assert!(
        said.contains("warn  recipe.create"),
        "absolute path: {said}"
    );
    assert!(said.contains("warn  recipe.destroy"), "{said}");
    assert!(said.contains("destroy is explicitly disabled"), "{said}");
    assert!(said.contains("warn  smoke.destroy"), "{said}");

    let out = repo.by(&["recipe", "check", "broken"]);
    let said = text(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{said}");
    assert!(said.contains("fail  recipe.doctor.run"), "{said}");
    assert!(
        said.contains("doctor exited with code 4: vm-cli missing"),
        "{said}"
    );
    assert!(said.contains("warn  smoke "), "{said}");
    assert!(said.contains("skipped: the doctor failed"), "{said}");
    assert!(said.contains("recipe failed"), "{said}");

    // An invalid recipe is a configuration error.
    fs::write(&repo.user, "[recipes.Bad]\ncreate = \"x\"\n").unwrap();
    let out = repo.by(&["config", "validate"]);
    assert!(!out.status.success());
    assert!(text(&out.stdout).contains("recipes.Bad") || text(&out.stderr).contains("recipes.Bad"));
}

/// Build `bin` of `package` next to `by`; cargo exposes a binary's path
/// only to its own package's tests.
fn built(package: &str, bin: &str) -> PathBuf {
    let profile_dir = Path::new(BY).parent().unwrap().to_path_buf();
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Command::new(cargo);
    command
        .args(["build", "--quiet", "--offline", "--manifest-path"])
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml"))
        .args(["-p", package, "--bin", bin])
        .env("CARGO_TARGET_DIR", profile_dir.parent().unwrap());
    match profile_dir.file_name().and_then(|n| n.to_str()) {
        Some("debug") => {}
        Some("release") => {
            command.arg("--release");
        }
        Some(other) => {
            command.args(["--profile", other]);
        }
        None => panic!("unexpected binary location {BY}"),
    }
    assert!(command.status().unwrap().success(), "building {bin} failed");
    profile_dir.join(bin)
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", text(&out.stderr));
    text(&out.stdout)
}

impl Repo {
    /// With a first commit, so branches can be made.
    fn committed() -> Repo {
        let repo = Repo::new();
        fs::write(repo.root.join(".gitignore"), ".vms/\n").unwrap();
        git(&repo.root, &["add", "."]);
        git(
            &repo.root,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@localhost",
                "commit",
                "-q",
                "-m",
                "initial",
            ],
        );
        repo
    }

    /// The `--provider recipe:devbox` flags, with the machine's paths in
    /// this test's directory.
    fn recipe_flags(&self) -> Vec<String> {
        let base = self.root.parent().unwrap();
        vec![
            "--provider".into(),
            "recipe:devbox".into(),
            "--recipe-workdir".into(),
            base.join("vm-home/work").display().to_string(),
            "--recipe-home".into(),
            base.join("vm-home/home").display().to_string(),
        ]
    }

    fn run_on_recipe(&self, agent: &Path, prompt: &str, name: &str, extra: &[&str]) -> Output {
        let mut args: Vec<String> = [
            "run",
            prompt,
            "--name",
            name,
            "--harness",
            "gemini-cli",
            "--yes",
            "--command",
            agent.to_str().unwrap(),
        ]
        .map(str::to_owned)
        .to_vec();
        args.extend(self.recipe_flags());
        args.extend(extra.iter().map(|a| (*a).to_owned()));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut command = Command::new(BY);
        command
            .current_dir(&self.root)
            .args(&args)
            .env("BRANCHYARD_TRUST_FILE", &self.trust)
            .env("BRANCHYARD_USER_CONFIG", &self.user)
            .env("BRANCHYARD_SSH", &self.ssh)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("NO_COLOR", "1")
            .stdin(Stdio::null());
        for var in [
            "BRANCHYARD_BRANCH",
            "BRANCHYARD_REMOTE",
            "BRANCHYARD_DELEGATION",
            "BRANCHYARD_ROOT",
            "BRANCHYARD_BY",
        ] {
            command.env_remove(var);
        }
        command.output().unwrap()
    }
}

#[test]
fn by_run_on_a_recipe_machine_brings_the_work_back_and_rm_destroys_a_kept_one() {
    let repo = Repo::committed();
    let agent = built("branchyard-runtime", "fake-acp-agent");

    // Untrusted: refused before anything is created.
    let out = repo.run_on_recipe(&agent, "WRITE hello.txt=hi", "hello", &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    let said = text(&out.stderr);
    assert!(said.contains("has commands you have not trusted"), "{said}");
    assert!(said.contains("by recipe trust devbox"), "{said}");
    assert!(vms(&repo.root).is_empty());
    assert!(!repo.root.join(".branchyard/worktrees/hello").exists());

    let out = repo.by(&["recipe", "trust", "devbox"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let out = repo.run_on_recipe(&agent, "WRITE hello.txt=hi", "hello", &[]);
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(stderr.contains("machines recipe devbox makes"), "{stderr}");
    assert!(
        stdout.contains("wrote hello.txt") && stdout.contains("ready"),
        "{stdout}"
    );
    // The file the harness wrote on the machine is in the branch's diff.
    let out = repo.by(&["diff", "hello"]);
    let diff = text(&out.stdout);
    assert!(diff.contains("hello.txt") && diff.contains("+hi"), "{diff}");
    // Its machine was created and destroyed through the recipe.
    let made = vms(&repo.root);
    assert!(
        made.iter()
            .any(|n| n.starts_with("by-hello-") && n.ends_with(".destroyed")),
        "{made:?}"
    );

    // A kept machine is suspended between turns, and `by rm` destroys it.
    let out = repo.run_on_recipe(
        &agent,
        "WRITE kept.txt=k",
        "kept",
        &["--keep-sandbox", "pause"],
    );
    assert!(
        out.status.success(),
        "{}\n{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    let kept: Vec<String> = vms(&repo.root)
        .into_iter()
        .filter(|n| n.starts_with("by-kept-"))
        .collect();
    assert_eq!(kept.len(), 1, "created, not destroyed: {kept:?}");
    let records = repo.root.join(".branchyard/recipes");
    assert_eq!(fs::read_dir(&records).unwrap().count(), 1);
    let out = repo.by(&["rm", "kept"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        vms(&repo.root)
            .iter()
            .any(|n| n == &format!("{}.destroyed", kept[0])),
        "{:?}",
        vms(&repo.root)
    );
    assert_eq!(fs::read_dir(&records).unwrap().count(), 0);

    // A server does not run recipes: refused before anything is sent.
    let token = repo.root.parent().unwrap().join("token");
    fs::write(&token, "by_test_token\n").unwrap();
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
    let out = repo.by(&[
        "--remote",
        "http://127.0.0.1:9",
        "--token-file",
        token.to_str().unwrap(),
        "run",
        "go",
        "--provider",
        "recipe:devbox",
    ]);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("a server does not run environment recipes"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn defaults_can_choose_a_recipe_and_a_changed_recipe_is_refused_again() {
    let repo = Repo::committed();
    let agent = built("branchyard-runtime", "fake-acp-agent");
    let out = repo.by(&["recipe", "trust", "devbox"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let config = format!("{RECIPE}\n[defaults]\nprovider = \"recipe\"\nrecipe = \"devbox\"\n");
    fs::write(repo.root.join("branchyard.toml"), &config).unwrap();
    let out = repo.by(&["config", "validate"]);
    assert!(
        out.status.success(),
        "{}{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    let out = Command::new(BY)
        .current_dir(&repo.root)
        .args([
            "run",
            "WRITE d.txt=d",
            "--name",
            "by-default",
            "--harness",
            "gemini-cli",
            "--yes",
            "--command",
            agent.to_str().unwrap(),
        ])
        .env("BRANCHYARD_TRUST_FILE", &repo.trust)
        .env("BRANCHYARD_USER_CONFIG", &repo.user)
        .env("BRANCHYARD_SSH", &repo.ssh)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("NO_COLOR", "1")
        .env_remove("BRANCHYARD_BRANCH")
        .env_remove("BRANCHYARD_REMOTE")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    // The default workdir is under /tmp/branchyard on the machine; here
    // the "machine" is this host.
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(stderr.contains("machines recipe devbox makes"), "{stderr}");
    assert!(
        vms(&repo.root)
            .iter()
            .any(|n| n.starts_with("by-by-default-") && n.ends_with(".destroyed")),
        "{:?}",
        vms(&repo.root)
    );
    let worktree = repo.root.join(".branchyard/worktrees/by-default");
    let workdir =
        PathBuf::from(branchyard::RecipeOptions::default().workdir("by-default", &worktree));
    assert!(
        workdir.starts_with("/tmp/branchyard") && workdir.join("d.txt").is_file(),
        "{}",
        workdir.display()
    );
    let _ = fs::remove_dir_all(workdir.parent().unwrap());

    // A recipe changed since it was trusted is refused again.
    fs::write(
        repo.root.join("branchyard.toml"),
        config.replace("./vm/destroy.sh", "./vm/destroy.sh --force"),
    )
    .unwrap();
    let out = repo.run_on_recipe(&agent, "WRITE x=1", "changed", &[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("changed since you trusted it"),
        "{}",
        text(&out.stderr)
    );

    // `[defaults] recipe` alone, or naming no valid recipe, is refused.
    fs::write(
        repo.root.join("branchyard.toml"),
        format!("{RECIPE}\n[defaults]\nrecipe = \"devbox\"\n"),
    )
    .unwrap();
    let out = repo.by(&["config", "validate"]);
    assert!(!out.status.success());
    let said = format!("{}{}", text(&out.stdout), text(&out.stderr));
    assert!(said.contains("defaults.recipe"), "{said}");
}
