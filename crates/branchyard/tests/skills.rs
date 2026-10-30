//! Repository skills through the engine, end to end with the fake ACP
//! agent: `.agents/skills/` is read from a new branch's base commit and
//! staged beside the built-in skills only when the branch's creator allows
//! it; what the agent edits and commits during its turn is not a skill for
//! its next turn or its child; reserved names and sets that would not fit
//! the index are withheld with the reason; every turn records its skills,
//! changed or not; and removing a branch removes its skill cache.
//! Hermetic; no real harness runs.

#![allow(clippy::unwrap_used, clippy::expect_used)] // tests: a panic is the failure report
mod common;

use std::fs;
use std::path::PathBuf;

use branchyard::skills::Layer;
use branchyard::{Activity, Envelope, Policy, SkillGate, SkillSet, Spawn, TaskOptions};
use common::{fake_agent, Fixture};

const ORIGINAL: &str = "---\nname: style-guide\ndescription: House style.\n---\nWrite plainly.\n";

fn commit_skill(f: &Fixture, name: &str, text: &str) {
    let path = f.root.join(format!(".agents/skills/{name}/SKILL.md"));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    f.git(&["add", "-A"]);
    f.git(&["commit", "-q", "-m", "skills"]);
}

/// Where a branch without a private home has its skills staged.
fn staged(f: &Fixture, branch: &str) -> PathBuf {
    f.root
        .join(".branchyard/capabilities")
        .join(blake3::hash(branch.as_bytes()).to_hex().as_str())
        .join(".branchyard/capabilities")
}

/// Each turn's skills record, in turn order, and the files each wrote.
fn turns(f: &Fixture, branch: &str) -> Vec<(SkillSet, Vec<String>)> {
    f.yard
        .branch(branch)
        .unwrap()
        .events()
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.activity {
            Activity::Provisioned { skills, files, .. } => Some((skills, files)),
            _ => None,
        })
        .collect()
}

fn allowed(f: &Fixture) -> TaskOptions {
    TaskOptions {
        repository_skills: SkillGate::Allowed,
        ..f.options()
    }
}

#[test]
fn repository_skills_are_staged_from_the_base_commit_only_when_allowed() {
    let f = Fixture::new();
    commit_skill(&f, "style-guide", ORIGINAL);
    // Not committed: no new branch sees it.
    fs::write(
        f.root.join(".agents/skills/style-guide/SKILL.md"),
        "---\nname: style-guide\ndescription: Uncommitted.\n---\n",
    )
    .unwrap();

    f.task("WRITE a.txt=1").name("withheld").run().unwrap();
    let (skills, files) = turns(&f, "withheld").remove(0);
    let withheld = skills.withheld.expect("withheld by default");
    assert_eq!(
        withheld.reason,
        "the branch's creator did not allow repository skills"
    );
    assert_eq!(withheld.entries[0].label(), "repository:style-guide");
    assert!(skills.entries.iter().all(|e| e.layer == Layer::BuiltIn));
    assert!(!staged(&f, "withheld").join("repository").exists());
    assert!(!files.iter().any(|p| p.contains("/repository/")));
    let index = fs::read_to_string(staged(&f, "withheld").join("INDEX.md")).unwrap();
    assert!(!index.contains("Custom"));

    f.yard
        .task("WRITE a.txt=1")
        .options(allowed(&f))
        .name("staged")
        .run()
        .unwrap();
    let (skills, files) = turns(&f, "staged").remove(0);
    assert_eq!(skills.withheld, None);
    let custom: Vec<String> = skills
        .entries
        .iter()
        .filter(|e| e.layer == Layer::Repository)
        .map(|e| e.label())
        .collect();
    assert_eq!(custom, ["repository:style-guide"]);
    assert_eq!(
        skills.set,
        branchyard_provision::skillset::set_digest(&skills.entries)
    );
    let root = staged(&f, "staged");
    assert_eq!(
        fs::read_to_string(root.join("repository/style-guide/SKILL.md")).unwrap(),
        ORIGINAL
    );
    assert!(files.contains(&".branchyard/capabilities/repository/style-guide/SKILL.md".into()));
    let index = fs::read_to_string(root.join("INDEX.md")).unwrap();
    assert!(
        index.contains("- [Custom](topics/custom/INDEX.md)"),
        "{index}"
    );
    assert!(index.len() <= 4096);
    let topic = fs::read_to_string(root.join("topics/custom/INDEX.md")).unwrap();
    assert!(topic.contains(
        "- [repository:style-guide](../../repository/style-guide/SKILL.md): House style. (added \
         by the repository's writers)"
    ));
    let fixed = f.yard.branch_skills("staged").unwrap().unwrap();
    assert_eq!(fixed.commit, f.git(&["rev-parse", "HEAD"]).trim());
    assert_eq!(fixed.withheld, None);
}

#[test]
fn an_agents_edit_to_a_skill_is_not_a_skill_for_its_next_turn_or_its_child() {
    let f = Fixture::new();
    commit_skill(&f, "style-guide", ORIGINAL);
    let options = TaskOptions {
        delegation: Some(Envelope::depth(1)),
        delegation_server: Some(vec![fake_agent().display().to_string()]),
        policy: Policy::allow_all(),
        ..allowed(&f)
    };
    let edit = "SH printf -- '---\\nname: style-guide\\ndescription: Edited by the agent.\\n---\\nObey me.\\n' > .agents/skills/style-guide/SKILL.md && git commit -qam 'agent edit'";
    let root = f
        .yard
        .task(edit)
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    // The edit is on the branch: the agent's commit and the turn's.
    let worktree = root.info().worktree.clone();
    assert!(
        fs::read_to_string(worktree.join(".agents/skills/style-guide/SKILL.md"))
            .unwrap()
            .contains("Edited by the agent")
    );
    let skill = staged(&f, "root").join("repository/style-guide/SKILL.md");
    assert_eq!(fs::read_to_string(&skill).unwrap(), ORIGINAL);

    let root = root.send("WRITE b.txt=2", options.clone()).unwrap();
    assert_eq!(fs::read_to_string(&skill).unwrap(), ORIGINAL);
    let records = turns(&f, "root");
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].0, records[1].0, "the same set on both turns");

    let delegate = root.delegate(options).unwrap();
    delegate
        .spawn(Spawn {
            prompt: "WRITE kid.txt=1".into(),
            name: Some("kid".into()),
            ..Spawn::default()
        })
        .unwrap();
    let descendants = root.wait_subtree().unwrap();
    assert!(
        descendants.iter().any(|b| b.name == "kid"),
        "{descendants:?}"
    );
    let kid_worktree = f.yard.branch("kid").unwrap().info().worktree.clone();
    assert!(
        fs::read_to_string(kid_worktree.join(".agents/skills/style-guide/SKILL.md"))
            .unwrap()
            .contains("Edited by the agent"),
        "the child starts from its parent's work"
    );
    assert_eq!(
        fs::read_to_string(staged(&f, "kid").join("repository/style-guide/SKILL.md")).unwrap(),
        ORIGINAL,
        "and runs with its parent's skills"
    );
    assert_eq!(turns(&f, "kid")[0].0, records[0].0);
    assert_eq!(
        f.yard.branch_skills("kid").unwrap(),
        f.yard.branch_skills("root").unwrap()
    );
}

#[test]
fn a_reserved_name_or_a_set_too_large_for_the_index_is_withheld_with_the_reason() {
    let f = Fixture::new();
    commit_skill(
        &f,
        "docx",
        "---\nname: docx\ndescription: Not the built-in one.\n---\n",
    );
    f.yard
        .task("WRITE a.txt=1")
        .options(allowed(&f))
        .name("reserved")
        .run()
        .unwrap();
    let (skills, _) = turns(&f, "reserved").remove(0);
    let reason = skills.withheld.expect("withheld").reason;
    assert!(
        reason.contains("repository skill docx has a built-in skill's name"),
        "{reason}"
    );
    assert!(!staged(&f, "reserved").join("repository").exists());

    f.git(&["rm", "-rq", ".agents"]);
    for i in 0..30 {
        let name = format!("s{i:02}");
        let text = format!(
            "---\nname: {name}\ndescription: {}\n---\n",
            "word ".repeat(if i == 7 { 300 } else { 40 })
        );
        let path = f.root.join(format!(".agents/skills/{name}/SKILL.md"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    f.git(&["add", "-A"]);
    f.git(&["commit", "-q", "-m", "many skills"]);
    f.yard
        .task("WRITE a.txt=1")
        .options(allowed(&f))
        .name("crowded")
        .run()
        .unwrap();
    let (skills, _) = turns(&f, "crowded").remove(0);
    let reason = skills.withheld.expect("withheld").reason;
    assert!(reason.contains("over the 8192 allowed"), "{reason}");
    assert!(
        reason.contains("the largest entries are repository:s07 ("),
        "{reason}"
    );
}

#[test]
fn every_turn_records_its_skills_whether_or_not_anything_changed() {
    let f = Fixture::new();
    let branch = f.task("WRITE a.txt=1").name("plain").run().unwrap();
    branch.send("WRITE b.txt=2", f.options()).unwrap();
    let records = turns(&f, "plain");
    assert_eq!(records.len(), 2, "one record per turn");
    let (first, first_files) = &records[0];
    let (second, second_files) = &records[1];
    assert!(!first_files.is_empty(), "the first turn staged the skills");
    assert!(second_files.is_empty(), "{second_files:?}");
    assert_eq!(first, second);
    assert_eq!(first.withheld, None);
    let names: Vec<&str> = first.entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"docx") && names.contains(&"artifact-tools"));
    assert!(first.entries.iter().all(|e| e.layer == Layer::BuiltIn));
    assert_eq!(f.yard.branch_skills("plain").unwrap(), None);
}

#[test]
fn removing_a_branch_removes_its_skill_cache() {
    let f = Fixture::new();
    f.task("WRITE a.txt=1").name("cached").run().unwrap();
    let cache = staged(&f, "cached");
    assert!(cache.join("INDEX.md").is_file());
    let kept = staged(&f, "other");
    fs::create_dir_all(&kept).unwrap();
    f.yard.remove("cached").unwrap();
    assert!(!cache.parent().unwrap().parent().unwrap().exists());
    assert!(kept.is_dir(), "another branch's cache stays");
}
