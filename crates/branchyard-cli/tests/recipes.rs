//! `by recipe` end to end: trust gating like `[workspace]`'s, and `check`
//! (the doctor, then create, exec over ssh, suspend, resume and destroy)
//! against a recipe whose scripts make a local "VM" reached through
//! `crates/branchyard-recipe/tests/fixtures/fake-ssh`. No cloud, no sshd.
//! Requires `git`, `sh`, `python3`, `ps`.

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
