//! A served repository's `[workspace.env]` cannot expand the server's
//! AIPlex token into its harness or its scripts: the table is the
//! repository's, the token is the server's. A file of its own: it sets a
//! variable of this test process, once.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::fs;

use branchyard_client::api::OperationState;
use branchyard_server::config::WorkspaceScripts;
use branchyard_setup::config::{ModelBackendConfig, ModelRouteConfig, ModelsConfig};
use common::{run, task, Fixture, Server};

const AIPLEX_TOKEN: &str = "server-aiplex-token-0123456789";

#[test]
fn a_served_repositorys_table_cannot_expand_the_servers_token() {
    std::env::set_var("BY_TEST_SERVER_AIPLEX_TOKEN", AIPLEX_TOKEN);
    let f = Fixture::new();
    fs::write(f.root.join(".gitignore"), "branchyard.toml\n").unwrap();
    common::git(&f.root, &["add", ".gitignore"]);
    common::git(&f.root, &["commit", "-q", "-m", "ignore"]);
    let (setup, harness) = (f.dir.join("setup.env"), f.dir.join("harness.env"));
    fs::write(
        f.root.join("branchyard.toml"),
        format!(
            "[workspace]\nsetup = \"env > '{}'\"\n\n[workspace.env]\n\
             MY_TOKEN = \"$BY_TEST_SERVER_AIPLEX_TOKEN\"\n\
             MY_HEADER = \"Bearer ${{BY_TEST_SERVER_AIPLEX_TOKEN}}\"\nMY_OTHER = \"fine\"\n",
            setup.display()
        ),
    )
    .unwrap();
    let mut config = f.config();
    config.allow_workspace_scripts = WorkspaceScripts::Repos(["app".to_owned()].into());
    config.models = Some(ModelsConfig {
        backends: [(
            "ap".to_owned(),
            ModelBackendConfig {
                api: "anthropic".into(),
                url: Some("https://aiplex.example.com/ap/ide".into()),
                key: Some("BY_TEST_SERVER_AIPLEX_TOKEN".into()),
                control_plane: Some("aiplex".into()),
                ..ModelBackendConfig::default()
            },
        )]
        .into(),
        routes: vec![ModelRouteConfig {
            model: "claude-*".into(),
            backends: vec!["ap".into()],
            ..ModelRouteConfig::default()
        }],
        ..ModelsConfig::default()
    });
    let server = Server::start(config);
    let client = server.client();
    let done = run(
        &client,
        &task(&format!("SH env > '{}'", harness.display()), "table"),
    );
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    // Neither the setup command nor the harness holds the token, under any
    // name; the entry with a value of its own is theirs.
    for file in [&setup, &harness] {
        let env = fs::read_to_string(file)
            .unwrap_or_else(|e| format!("{} was not written: {e}", file.display()));
        // A failure shows the variables' names, never the environment
        // itself, which is the machine's that runs this test.
        let names = env
            .lines()
            .filter_map(|line| line.split_once('=').map(|(name, _)| name))
            .collect::<Vec<_>>()
            .join(",");
        assert!(!env.contains(AIPLEX_TOKEN), "{}: {names}", file.display());
        assert!(env.contains("MY_OTHER=fine"), "{}: {names}", file.display());
    }
    // The branch's log names the entries and the backend, never the value.
    let events = client.repo("app").events("table", 0).unwrap();
    let warnings: Vec<&str> = events
        .events
        .iter()
        .filter_map(|event| match &event.activity {
            branchyard::Activity::Warning(warning) => Some(warning.as_str()),
            _ => None,
        })
        .collect();
    for entry in ["MY_TOKEN", "MY_HEADER"] {
        let warning = format!(
            "{entry} was not given to the harness: its value holds the token of the \
             control-plane backend ap, which no harness is given"
        );
        assert!(warnings.contains(&warning.as_str()), "{warnings:?}");
    }
    assert!(!format!("{events:?}").contains(AIPLEX_TOKEN));
}
