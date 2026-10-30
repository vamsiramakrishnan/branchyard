//! A served repository's own skills (`.agents/skills/` at a new branch's
//! base commit) are staged only when the server's operator allows them
//! (`allow_repository_skills`), never because a request or the
//! repository's own branchyard.toml asks, and a configuration naming a
//! repository the server does not serve is refused.

#![allow(clippy::unwrap_used, clippy::expect_used)] // tests: a panic is the failure report
mod common;

use std::fs;

use branchyard::skills::Layer;
use branchyard::{Activity, SkillSet};
use branchyard_client::api::OperationState;
use branchyard_client::Client;
use branchyard_server::config::WorkspaceScripts;
use common::{run, task, Fixture, Server, TOKEN};

const SKILL: &str = ".agents/skills/style-guide/SKILL.md";

fn fixture() -> Fixture {
    let f = Fixture::new();
    let skill = f.root.join(SKILL);
    fs::create_dir_all(skill.parent().unwrap()).unwrap();
    fs::write(
        &skill,
        "---\nname: style-guide\ndescription: House style.\n---\nWrite plainly.\n",
    )
    .unwrap();
    common::git(&f.root, &["add", "-A"]);
    common::git(&f.root, &["commit", "-q", "-m", "skills"]);
    f
}

/// The skills record of `branch`'s first turn, and the files it staged.
fn provisioned(client: &Client, branch: &str) -> (SkillSet, Vec<String>) {
    let events = client.repo("app").events(branch, 0).unwrap();
    events
        .events
        .iter()
        .find_map(|e| match &e.activity {
            Activity::Provisioned { skills, files, .. } => Some((skills.clone(), files.clone())),
            _ => None,
        })
        .expect("a provisioned activity")
}

#[test]
fn a_repositorys_skills_are_staged_only_when_the_operator_allows_them() {
    let f = fixture();
    let server = Server::start(f.config());
    let client = server.client();
    let done = run(&client, &task("WRITE x.txt=1", "plain"));
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    let (skills, files) = provisioned(&client, "plain");
    assert!(skills.entries.iter().all(|e| e.layer == Layer::BuiltIn));
    assert!(!skills.entries.is_empty());
    let withheld = skills.withheld.expect("withheld");
    assert!(
        withheld.reason.contains("allow_repository_skills"),
        "{}",
        withheld.reason
    );
    assert_eq!(withheld.entries[0].label(), "repository:style-guide");
    assert!(
        !files.iter().any(|p| p.contains("/repository/")),
        "{files:?}"
    );
    drop(server);

    let mut config = f.config();
    config.allow_repository_skills = WorkspaceScripts::Repos(["app".to_owned()].into());
    let server = Server::start(config);
    let client = server.client();
    let done = run(&client, &task("WRITE x.txt=1", "allowed"));
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    let (skills, files) = provisioned(&client, "allowed");
    assert_eq!(skills.withheld, None);
    assert!(skills
        .entries
        .iter()
        .any(|e| e.label() == "repository:style-guide"));
    assert!(
        files
            .iter()
            .any(|p| p == ".branchyard/capabilities/repository/style-guide/SKILL.md"),
        "{files:?}"
    );
}

#[test]
fn a_repositorys_own_configuration_cannot_allow_its_skills() {
    let refused = branchyard_setup::config::parse("allow_repository_skills = true\n");
    assert!(refused.is_err(), "{refused:?}");
    let f = fixture();
    fs::write(
        f.root.join("branchyard.toml"),
        "allow_repository_skills = true\n",
    )
    .unwrap();
    common::git(&f.root, &["add", "-A"]);
    common::git(&f.root, &["commit", "-q", "-m", "ask"]);
    let server = Server::start(f.config());
    let client = server.client();
    let done = run(&client, &task("WRITE x.txt=1", "asks"));
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    let (skills, _) = provisioned(&client, "asks");
    assert!(skills.withheld.is_some(), "{skills:?}");
}

#[test]
fn allowing_skills_of_a_repository_the_server_does_not_serve_is_refused() {
    let f = fixture();
    let mut config = f.config();
    config.allow_repository_skills = WorkspaceScripts::Repos(["elsewhere".to_owned()].into());
    let error = config.validate().unwrap_err();
    assert!(
        error.contains("allow_repository_skills names elsewhere"),
        "{error}"
    );
    let path = f.dir.join("server.json");
    fs::write(
        &path,
        format!(
            r#"{{"repos": {{"app": "{}"}}, "data_dir": "{}", "allow_repository_skills": true,
                "tokens": [{{"name": "t", "token": "{TOKEN}"}}]}}"#,
            f.root.display(),
            f.data.display()
        ),
    )
    .unwrap();
    let partial = branchyard_server::config::load_file(&path).unwrap();
    assert_eq!(partial.allow_repository_skills, WorkspaceScripts::All);
}
