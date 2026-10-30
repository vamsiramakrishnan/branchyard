//! `by skills` through the built `by`, against temporary repositories and
//! the fake ACP agent: `ls` with no skills, with a set not trusted, trusted,
//! changed since it was trusted and forgotten; the uncommitted note; `trust`
//! refused inside a harness, without skills and for a set the engine
//! refuses; `--remote` refused; and new branches staging the repository's
//! skills only once trusted, as their `Provisioned` records and `by skills
//! ls BRANCH` say. Hermetic; requires `git` and `sh`.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
use std::fs;
use std::path::PathBuf;
use std::process::Output;

use serde_json::Value;

const STYLE: &str = "---\nname: style-guide\ndescription: Write prose the way this team \
                     does.\n---\n\nUse short sentences.\n";

/// The kit's repository, plus what this file adds.
struct Repo(branchyard_testkit::Repo);

impl std::ops::Deref for Repo {
    type Target = branchyard_testkit::Repo;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Repo {
    /// A repository whose first commit holds the style-guide skill.
    fn with_skill() -> Repo {
        Repo(branchyard_testkit::repo!(&[
            ("a.txt", "one\n"),
            (".agents/skills/style-guide/SKILL.md", STYLE)
        ]))
    }

    fn without_skills() -> Repo {
        Repo(branchyard_testkit::repo!())
    }

    fn trust_file(&self) -> PathBuf {
        self.dir.join("user/trusted-workspaces.json")
    }

    /// Write `text` to the skill's SKILL.md and commit it.
    fn commit_skill(&self, text: &str) {
        fs::write(self.skill_md(), text).unwrap();
        self.git(&["commit", "-qam", "change the skill"]);
    }

    fn skill_md(&self) -> PathBuf {
        self.root.join(".agents/skills/style-guide/SKILL.md")
    }

    /// `by <args>` as a harness on `branch` would run it.
    fn in_harness(&self, branch: &str, args: &[&str]) -> Output {
        self.command(env!("CARGO_BIN_EXE_by"))
            .args(args)
            .env("BRANCHYARD_BRANCH", branch)
            .output()
            .unwrap()
    }

    /// The `provisioned` activity of `branch`'s first turn, from `by log
    /// --json`.
    fn provisioned(&self, branch: &str) -> Value {
        let log = self.json(&["log", branch, "--json"]);
        log.as_array()
            .unwrap()
            .iter()
            .find(|e| e["activity"] == "provisioned")
            .cloned()
            .unwrap_or_else(|| panic!("{branch} recorded no provisioned activity: {log}"))
    }

    /// Each branch cache's staged repository skills directory that exists:
    /// `<state>/capabilities/<branch hash>/.branchyard/capabilities/repository`.
    fn staged_repository_dirs(&self) -> Vec<PathBuf> {
        let caches = self.root.join(".branchyard/capabilities");
        let mut found: Vec<PathBuf> = fs::read_dir(&caches)
            .unwrap_or_else(|e| panic!("{}: {e}", caches.display()))
            .map(|entry| {
                entry
                    .unwrap()
                    .path()
                    .join(".branchyard/capabilities/repository")
            })
            .filter(|dir| dir.exists())
            .collect();
        found.sort();
        found
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The envelope a failed `--json` command printed on standard output.
fn envelope(out: &Output) -> Value {
    let value: Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("stdout is not JSON ({e}):\n{}", stdout(out)));
    value["error"].clone()
}

#[test]
fn ls_shows_the_built_in_skills_and_no_repository_skills() {
    let repo = Repo::without_skills();
    let shown = repo.json(&["skills", "ls", "--json"]);
    assert_eq!(shown["repository"]["state"], "none", "{shown}");
    assert_eq!(shown["repository"]["skills"], serde_json::json!([]));
    assert_eq!(shown["uncommitted"], false);
    let built_in = shown["built_in"].as_array().unwrap();
    assert!(!built_in.is_empty(), "{shown}");
    assert!(built_in.iter().all(|e| e["layer"] == "built_in"), "{shown}");
    assert_eq!(built_in[0]["digest"].as_str().unwrap().len(), 64);

    let text = stdout(&repo.ok(&["skills", "ls"]));
    assert!(
        text.contains(&format!("built-in    {} skills: ", built_in.len())),
        "{text}"
    );
    assert!(
        text.contains(
            "repository  none: no .agents/skills/<name>/SKILL.md at HEAD (docs/knowledge-skills.md)"
        ),
        "{text}"
    );
}

#[test]
fn trust_follows_the_set_at_head_through_a_change_and_untrust() {
    let repo = Repo::with_skill();
    let shown = repo.json(&["skills", "ls", "--json"]);
    let repository = &shown["repository"];
    assert_eq!(repository["state"], "untrusted", "{shown}");
    let first_set = repository["set"].as_str().unwrap().to_owned();
    assert_eq!(first_set.len(), 64);
    assert_eq!(
        repository["commit"].as_str().unwrap(),
        repo.git(&["rev-parse", "HEAD"]).trim()
    );
    let skill = &repository["skills"][0];
    assert_eq!(skill["label"], "repository:style-guide");
    assert_eq!(skill["name"], "style-guide");
    assert_eq!(skill["description"], "Write prose the way this team does.");
    assert_eq!(skill["digest"].as_str().unwrap().len(), 64);
    let text = stdout(&repo.ok(&["skills", "ls"]));
    assert!(
        text.contains(&format!(
            "repository  1 skill at commit {} in .agents/skills, set {}\n  \
             repository:style-guide  {}  Write prose the way this team does.\n",
            &repository["commit"].as_str().unwrap()[..12],
            &first_set[..12],
            &skill["digest"].as_str().unwrap()[..12]
        )),
        "{text}"
    );
    assert!(
        text.contains(
            "trust       not trusted: new branches here withhold them; review them, then by \
             skills trust\n"
        ),
        "{text}"
    );

    let trusted = repo.json(&["skills", "trust", "--json"]);
    assert_eq!(trusted["set"], first_set.as_str());
    assert_eq!(trusted["trusted"], true);
    assert_eq!(
        trusted["skills"],
        serde_json::json!(["repository:style-guide"])
    );
    assert_eq!(
        trusted["trust_file"],
        repo.trust_file().display().to_string()
    );
    let file: Value =
        serde_json::from_str(&fs::read_to_string(repo.trust_file()).unwrap()).unwrap();
    let root = fs::canonicalize(&repo.root).unwrap().display().to_string();
    assert_eq!(file["skills"][&root]["digest"], first_set.as_str());
    assert_eq!(
        repo.json(&["skills", "ls", "--json"])["repository"]["state"],
        "trusted"
    );
    let text = stdout(&repo.ok(&["skills", "trust"]));
    assert_eq!(
        text,
        format!(
            "trusted 1 repository skill at commit {} (set {}): repository:style-guide\nnew \
             branches of {root} stage them; after a change, run by skills trust again\n",
            &trusted["commit"].as_str().unwrap()[..12],
            &first_set[..12]
        )
    );

    fs::write(repo.skill_md(), STYLE.replace("short", "plain")).unwrap();
    let out = repo.ok(&["skills", "ls", "--json"]);
    assert_eq!(
        stderr(&out),
        "by: .agents/skills has uncommitted changes; only committed skills count\n"
    );
    let shown: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(shown["uncommitted"], true);
    assert_eq!(shown["repository"]["state"], "trusted");

    repo.git(&["commit", "-qam", "change the skill"]);
    let shown = repo.json(&["skills", "ls", "--json"]);
    assert_eq!(shown["repository"]["state"], "changed", "{shown}");
    assert_eq!(shown["repository"]["trusted_set"], first_set.as_str());
    assert_ne!(shown["repository"]["set"], first_set.as_str());
    let text = stdout(&repo.ok(&["skills", "ls"]));
    assert!(
        text.contains(&format!(
            "trust       changed since you trusted set {}: new branches here withhold them; \
             review them, then by skills trust\n",
            &first_set[..12]
        )),
        "{text}"
    );
    repo.ok(&["skills", "trust"]);
    assert_eq!(
        repo.json(&["skills", "ls", "--json"])["repository"]["state"],
        "trusted"
    );

    let removed = repo.json(&["skills", "untrust", "--json"]);
    assert_eq!(removed["removed"], true);
    assert_eq!(removed["root"], root.as_str());
    assert_eq!(
        repo.json(&["skills", "ls", "--json"])["repository"]["state"],
        "untrusted"
    );
    assert_eq!(
        stdout(&repo.ok(&["skills", "untrust"])),
        format!("{root}'s repository skills were not trusted\n")
    );
    repo.ok(&["skills", "trust"]);
    assert_eq!(
        stdout(&repo.ok(&["skills", "untrust"])),
        format!("{root}'s repository skills are no longer trusted; new branches withhold them\n")
    );
    assert_eq!(
        repo.json(&["skills", "untrust", "--json"])["removed"],
        false
    );
}

#[test]
fn a_harness_on_a_branch_can_never_trust_skills() {
    let repo = Repo::with_skill();
    let out = repo.in_harness("some-branch", &["skills", "trust"]);
    assert_eq!(out.status.code(), Some(4));
    assert_eq!(
        stderr(&out),
        "by: a harness running on a branch cannot trust repository skills; trusting is a \
         person's decision, made outside any branch\n  next: run by skills trust in your own \
         terminal\n"
    );
    let out = repo.in_harness("some-branch", &["skills", "trust", "--json"]);
    assert_eq!(out.status.code(), Some(4));
    let error = envelope(&out);
    assert_eq!(error["kind"], "denied");
    assert_eq!(error["exit"], 4);
    assert!(!repo.trust_file().exists());
}

#[test]
fn trust_refuses_a_repository_without_skills_and_a_set_the_engine_refuses() {
    let repo = Repo::without_skills();
    let root = fs::canonicalize(&repo.root).unwrap().display().to_string();
    let out = repo.by(&["skills", "trust"]);
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(
        stderr(&out),
        format!(
            "by: {root} has no skills to trust at HEAD\n  next: add \
             .agents/skills/<name>/SKILL.md and commit it (docs/knowledge-skills.md)\n"
        )
    );
    let error = envelope(&repo.by(&["skills", "trust", "--json"]));
    assert_eq!(error["kind"], "config");
    assert_eq!(error["exit"], 3);

    let repo = Repo::with_skill();
    repo.commit_skill(&STYLE.replace("name: style-guide", "name: other"));
    let commit = repo.git(&["rev-parse", "HEAD"]);
    let reason = format!(
        "the repository's skills at {} are refused: skill style-guide: SKILL.md names \
         \"other\"; the name must be its directory's",
        &commit[..12]
    );
    let out = repo.by(&["skills", "trust"]);
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(
        stderr(&out),
        format!("by: {reason}\n  next: fix it and commit, then by skills trust\n")
    );
    assert!(!repo.trust_file().exists());
    let shown = repo.json(&["skills", "ls", "--json"]);
    assert_eq!(shown["repository"]["state"], "refused");
    assert_eq!(shown["repository"]["reason"], reason.as_str());
    let text = stdout(&repo.ok(&["skills", "ls"]));
    assert!(
        text.contains(&format!(
            "repository  refused: {reason}; fix it and commit, then by skills trust\n"
        )),
        "{text}"
    );
}

#[test]
fn skills_refuses_remote() {
    let repo = Repo::with_skill();
    let token = repo.dir.join("token");
    fs::write(&token, "secret\n").unwrap();
    let out = repo.by(&[
        "--remote",
        "http://127.0.0.1:1",
        "--token-file",
        token.to_str().unwrap(),
        // Named, so no server is asked which repositories it serves.
        "--repo",
        "app",
        "skills",
        "ls",
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    let error = envelope(&out);
    assert_eq!(error["kind"], "usage");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("only when its operator allows them (allow_repository_skills)"),
        "{error}"
    );
    assert_eq!(
        error["next"],
        serde_json::json!(["run by skills without --remote"])
    );
}

#[test]
fn new_branches_stage_the_repository_skills_only_once_trusted() {
    let repo = Repo::with_skill();
    let out = repo.by_agent(&["run", "WRITE f.txt=1", "--name", "untrusted"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let provisioned = repo.provisioned("untrusted");
    let skills = &provisioned["skills"];
    let reason = skills["withheld"]["reason"].as_str().unwrap();
    assert!(reason.contains("by skills ls"), "{provisioned}");
    assert!(reason.contains("by skills trust"), "{provisioned}");
    assert_eq!(
        skills["withheld"]["entries"][0]["name"], "style-guide",
        "{provisioned}"
    );
    let entries = skills["entries"].as_array().unwrap();
    assert!(!entries.is_empty(), "{provisioned}");
    assert!(
        entries.iter().all(|e| e["layer"] == "built_in"),
        "{provisioned}"
    );
    assert!(repo.staged_repository_dirs().is_empty());
    let shown = repo.json(&["skills", "ls", "untrusted", "--json"]);
    assert_eq!(shown["branch"], "untrusted");
    assert_eq!(shown["repository"]["state"], "withheld", "{shown}");
    assert_eq!(shown["repository"]["reason"], reason);
    assert_eq!(
        shown["repository"]["skills"][0]["label"],
        "repository:style-guide"
    );
    let log = stdout(&repo.ok(&["log", "untrusted"]));
    assert!(
        log.contains(&format!("repository skills withheld: {reason}")),
        "{log}"
    );

    repo.ok(&["skills", "trust"]);
    let out = repo.by_agent(&["run", "WRITE g.txt=1", "--name", "trusted"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let provisioned = repo.provisioned("trusted");
    let skills = &provisioned["skills"];
    assert!(skills.get("withheld").is_none(), "{provisioned}");
    let entries = skills["entries"].as_array().unwrap();
    let ours = entries
        .iter()
        .find(|e| e["layer"] == "repository")
        .unwrap_or_else(|| panic!("no repository entry: {provisioned}"));
    assert_eq!(ours["name"], "style-guide");
    assert!(
        provisioned["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == ".branchyard/capabilities/repository/style-guide/SKILL.md"),
        "{provisioned}"
    );
    let staged = repo.staged_repository_dirs();
    assert_eq!(staged.len(), 1, "{staged:?}");
    assert_eq!(
        fs::read_to_string(staged[0].join("style-guide/SKILL.md")).unwrap(),
        STYLE
    );

    let shown = repo.json(&["skills", "ls", "trusted", "--json"]);
    assert_eq!(shown["repository"]["state"], "staged", "{shown}");
    assert!(shown["repository"].get("reason").is_none());
    assert_eq!(
        shown["repository"]["skills"][0]["label"],
        "repository:style-guide"
    );
    assert_eq!(shown["repository"]["skills"][0]["digest"], ours["digest"]);
    let text = stdout(&repo.ok(&["skills", "ls", "trusted"]));
    assert!(text.starts_with("branch      trusted\n"), "{text}");
    assert!(text.contains("state       staged\n"), "{text}");
    let log = stdout(&repo.ok(&["log", "trusted"]));
    assert!(log.contains("repository:style-guide"), "{log}");

    let out = repo.in_harness("trusted", &["skills", "ls", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let own: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(own, shown);
}
