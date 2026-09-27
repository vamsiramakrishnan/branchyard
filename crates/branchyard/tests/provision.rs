//! Provisioning through the engine, end to end with the fake ACP agent:
//! the harness sees what was provisioned in its private home and
//! environment, secrets stay out of the worktree, the event log and the
//! state database, a user's edits survive the next turn, and requests that
//! cannot be honored are refused before anything is created. Hermetic; no
//! real harness runs.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use branchyard::{
    Activity, BranchStatus, Effort, McpServerSpec, Policy, Provisioning, RemoteMcpSpec,
    RemoteMcpTransport, RemoveOptions, SecretSource, TaskOptions, Via,
};
use common::{edit_record, stored_record, text, Fixture};

const OPENAI_SECRET: &str = "sk-proj-PROVISIONED-SECRET-0123456789abcdefXYZ";

/// Options that run the fake agent as the `codex-acp` profile, isolated,
/// with an API key read from `var`.
fn codex(f: &Fixture, var: &str, value: &str) -> TaskOptions {
    std::env::set_var(var, value);
    TaskOptions {
        harness: Some("codex-acp".into()),
        isolated: true,
        provision: Some(Provisioning {
            secrets: vec![SecretSource::parse(&format!("OPENAI_API_KEY={var}")).unwrap()],
            effort: Some(Effort::High),
            model: Some("gpt-5.5-codex".into()),
            ..Provisioning::default()
        }),
        ..f.options()
    }
}

/// Every file under `dir` whose bytes contain `needle`, skipping `skip`.
fn containing(dir: &Path, needle: &str, skip: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        if current.starts_with(skip) {
            continue;
        }
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                let bytes = fs::read(&path).unwrap_or_default();
                if bytes.windows(needle.len()).any(|w| w == needle.as_bytes()) {
                    found.push(path);
                }
            }
        }
    }
    found
}

#[test]
fn the_harness_sees_its_provisioned_home_and_the_secret_stays_there() {
    let f = Fixture::new();
    let branch = f
        .task(
            "SH stat -c '%a %n' \"$HOME/.codex/auth.json\"\n\
             SH cat \"$HOME/.codex/config.toml\"\n\
             SH test \"$CODEX_HOME\" = \"$HOME/.codex\" && echo codex-home-ok",
        )
        .options(codex(&f, "BY_TEST_OPENAI_ONE", OPENAI_SECRET))
        .name("provisioned")
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    let events = branch.events().unwrap();
    assert_eq!(branch.info().status, BranchStatus::NoChanges, "{events:?}");
    let said = text(&events);
    assert!(said.contains("600 "), "{said}");
    assert!(said.contains("model_reasoning_effort = \"high\""), "{said}");
    assert!(said.contains("model = \"gpt-5.5-codex\""), "{said}");
    assert!(said.contains("codex-home-ok"), "{said}");
    assert!(!branch.info().worktree.join(".codex").exists());

    // Provisioning was recorded, by name.
    let provisioned = events
        .iter()
        .find_map(|e| match &e.activity {
            Activity::Provisioned {
                auth, files, env, ..
            } => Some((auth.clone(), files.clone(), env.clone())),
            _ => None,
        })
        .expect("a provisioned activity");
    assert_eq!(provisioned.0.as_deref(), Some("api-key"));
    assert_eq!(
        provisioned.1,
        [
            ".codex/auth.json",
            ".codex/config.toml",
            ".branchyard/provisioned.json"
        ]
    );
    assert_eq!(provisioned.2, ["CODEX_HOME"]);

    // The secret is in the private home and nowhere else: not the
    // worktree, the candidate, the event log, the stored record, nor the
    // state database.
    let home = PathBuf::from(
        stored_record(&f.root, "provisioned")["home"]
            .as_str()
            .unwrap(),
    );
    assert!(fs::read_to_string(home.join(".codex/auth.json"))
        .unwrap()
        .contains(OPENAI_SECRET));
    assert_eq!(
        containing(&f.dir, OPENAI_SECRET, &home),
        Vec::<PathBuf>::new()
    );
    let log = serde_json::to_string(&events).unwrap();
    assert!(!log.contains(OPENAI_SECRET));
    let record = stored_record(&f.root, "provisioned").to_string();
    assert!(!record.contains(OPENAI_SECRET));
    assert!(
        record.contains("BY_TEST_OPENAI_ONE"),
        "the source is kept: {record}"
    );
}

#[test]
fn a_send_provisions_again_and_keeps_what_the_user_added() {
    let f = Fixture::new();
    let branch = f
        .task("WRITE a.txt=1")
        .options(codex(&f, "BY_TEST_OPENAI_TWO", OPENAI_SECRET))
        .name("resent")
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    assert_eq!(branch.info().status, BranchStatus::Ready);
    let home = PathBuf::from(stored_record(&f.root, "resent")["home"].as_str().unwrap());
    let config = home.join(".codex/config.toml");
    let mut text_now = fs::read_to_string(&config).unwrap();
    text_now.push_str("\n[profiles.mine]\nmodel = \"o-mine\"\n");
    fs::write(&config, &text_now).unwrap();

    // The send keeps the branch's provisioning and resolves the secret
    // again; nothing that is already right is rewritten.
    let sent = branch
        .send(
            "SH cat \"$HOME/.codex/config.toml\"",
            TaskOptions {
                harness: Some("codex-acp".into()),
                ..f.options()
            },
        )
        .unwrap();
    let events = sent.events().unwrap();
    let said = text(&events);
    assert!(said.contains("[profiles.mine]"), "{said}");
    assert_eq!(said.matches("model_reasoning_effort").count(), 1, "{said}");
    let files: Vec<Vec<String>> = events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Provisioned { files, .. } => Some(files.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(files.last().unwrap(), &Vec::<String>::new(), "{files:?}");
}

#[test]
fn claude_code_acp_gets_its_key_model_and_mcp_server() {
    std::env::set_var(
        "BY_TEST_ANTHROPIC_KEY",
        "sk-ant-api03-provisioned-0123456789abcdefghij",
    );
    let f = Fixture::new();
    let server =
        McpServerSpec::parse(&format!("extra={} --stdio", common::fake_agent().display())).unwrap();
    let branch = f
        .task(
            "SH echo \"model=$ANTHROPIC_MODEL managed=$CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST\"\n\
             SH test -z \"$ANTHROPIC_API_KEY\" && echo key-not-in-env\n\
             SH test -z \"$BY_TEST_ANTHROPIC_KEY\" && echo source-not-in-env\n\
             SH grep -c apiKeyHelper \"$HOME/.claude/settings.json\"\n\
             SH stat -c 'key-file %a' \"$HOME/.branchyard/credentials/anthropic-api-key\"",
        )
        .options(TaskOptions {
            harness: Some("claude-code-acp".into()),
            isolated: true,
            provision: Some(Provisioning {
                secrets: vec![
                    SecretSource::parse("ANTHROPIC_API_KEY=BY_TEST_ANTHROPIC_KEY").unwrap(),
                ],
                model: Some("large".into()),
                mcp_servers: vec![server],
                ..Provisioning::default()
            }),
            ..f.options()
        })
        .name("claude-acp")
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    let said = text(&branch.events().unwrap());
    assert!(said.contains("model=claude-opus-5-5 managed=\n"), "{said}");
    assert!(said.contains("key-not-in-env"), "{said}");
    // Nor is the variable it was read from, which the harness would
    // otherwise inherit.
    assert!(said.contains("source-not-in-env"), "{said}");
    assert!(said.contains("key-file 600"), "{said}");
    assert!(!said.contains("0123456789abcdefghij"), "{said}");
    // The activity says how the key was delivered, and that no tool
    // command inherits it.
    let secrets = branch
        .events()
        .unwrap()
        .iter()
        .find_map(|e| match &e.activity {
            Activity::Provisioned { secrets, .. } => Some(secrets.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(secrets.len(), 1);
    assert_eq!(secrets[0].secret, "ANTHROPIC_API_KEY");
    assert!(matches!(secrets[0].via, Via::Helper { .. }) && !secrets[0].tool_env);
}

const MCP_SECRET: &str = "mcp-token-PROVISIONED-0123456789abcdefXYZ";
const CLAUDE_SECRET: &str = "sk-ant-api03-LAUNCH-SECRET-0123456789abcdefghij";
const HEADER_SECRET: &str = "Bearer remote-HEADER-SECRET-0123456789";

/// Options that run the fake agent as Claude Code's stream-json harness,
/// recording its launch in `record` and exiting, with an MCP server whose
/// variable holds a token.
fn claude_launch(f: &Fixture, record: &Path, isolated: bool) -> TaskOptions {
    let mut docs = McpServerSpec::parse("docs=/usr/bin/docs-mcp --stdio").unwrap();
    std::env::set_var("BY_TEST_CLAUDE_LAUNCH_KEY", CLAUDE_SECRET);
    std::env::set_var("BY_TEST_DOCS_TOKEN", MCP_SECRET);
    // With a private home, the server's token is a secret; without one,
    // where secrets are refused, a plain variable that is still kept off
    // the command line.
    std::env::set_var("BY_TEST_SEARCH_HEADER", HEADER_SECRET);
    let mut remote_mcp_servers = Vec::new();
    let secrets = match isolated {
        true => {
            docs.secret_env.insert("DOCS_TOKEN".into(), "DOCS".into());
            remote_mcp_servers.push(RemoteMcpSpec {
                name: "search".into(),
                transport: RemoteMcpTransport::Http,
                url: "https://mcp.example.invalid/mcp".into(),
                headers: [("Authorization".into(), "SEARCH".into())].into(),
            });
            vec![
                SecretSource::parse("ANTHROPIC_API_KEY=BY_TEST_CLAUDE_LAUNCH_KEY").unwrap(),
                SecretSource::parse("DOCS=BY_TEST_DOCS_TOKEN").unwrap(),
                SecretSource::parse("SEARCH=BY_TEST_SEARCH_HEADER").unwrap(),
            ]
        }
        false => {
            docs.env.insert("DOCS_TOKEN".into(), MCP_SECRET.into());
            Vec::new()
        }
    };
    TaskOptions {
        harness: Some("claude-code-stream-json".into()),
        command: Some(vec![
            common::fake_agent().display().to_string(),
            "--record-launch".into(),
            record.display().to_string(),
        ]),
        isolated,
        provision: Some(Provisioning {
            secrets,
            mcp_servers: vec![docs],
            remote_mcp_servers,
            ..Provisioning::default()
        }),
        ..f.options()
    }
}

fn launch(record: &Path) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(record).unwrap()).unwrap()
}

#[test]
fn no_secret_is_on_claude_codes_command_line_with_a_private_home() {
    let f = Fixture::new();
    let record = f.dir.join("launch.json");
    let branch = f
        .task("hello")
        .options(claude_launch(&f, &record, true))
        .name("launched")
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    let launched = launch(&record);
    let cmdline = launched["cmdline"].to_string();
    assert!(cmdline.contains("--mcp-config"), "{cmdline}");
    for secret in [MCP_SECRET, CLAUDE_SECRET, HEADER_SECRET] {
        assert!(!cmdline.contains(secret), "{cmdline}");
    }
    // The servers are in a 0600 file in the private home.
    let home = PathBuf::from(stored_record(&f.root, "launched")["home"].as_str().unwrap());
    let config = &launched["mcp_config"];
    assert_eq!(
        config["path"],
        home.join(".branchyard/claude-mcp.json")
            .display()
            .to_string()
    );
    assert_eq!(config["mode"], "600");
    assert_eq!(config["servers"], serde_json::json!(["docs", "search"]));
    // The API key is not in the harness's environment at all.
    let env = launched["env"].as_array().unwrap();
    assert!(
        !env.iter().any(|name| name == "ANTHROPIC_API_KEY"),
        "{env:?}"
    );
    // Every secret is in the private home and nowhere else: not the
    // state database, which holds only their names.
    for secret in [MCP_SECRET, CLAUDE_SECRET, HEADER_SECRET] {
        assert_eq!(containing(&f.dir, secret, &home), Vec::<PathBuf>::new());
        assert!(!containing(&home, secret, Path::new("/nonexistent")).is_empty());
    }
    let events = branch.events().unwrap();
    let log = serde_json::to_string(&events).unwrap();
    assert!(!log.contains(MCP_SECRET) && !log.contains(CLAUDE_SECRET));
    assert!(!log.contains(HEADER_SECRET));
    let secrets = events
        .iter()
        .find_map(|e| match &e.activity {
            Activity::Provisioned {
                secrets,
                unused_secrets,
                ..
            } => Some((secrets.clone(), unused_secrets.clone())),
            _ => None,
        })
        .unwrap();
    assert!(secrets.1.is_empty(), "{secrets:?}");
    assert!(secrets.0.iter().any(|d| d.secret == "DOCS"
        && d.via
            == Via::McpServer {
                server: "docs".into(),
                var: "DOCS_TOKEN".into()
            }));
    assert!(secrets.0.iter().any(|d| d.secret == "SEARCH"
        && d.via
            == Via::McpHeader {
                server: "search".into(),
                header: "Authorization".into()
            }));
}

#[test]
fn without_a_private_home_claude_codes_mcp_file_lasts_one_turn() {
    let f = Fixture::new();
    let record = f.dir.join("launch.json");
    f.task("hello")
        .options(claude_launch(&f, &record, false))
        .name("shared-home")
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    let launched = launch(&record);
    let cmdline = launched["cmdline"].to_string();
    assert!(!cmdline.contains(MCP_SECRET), "{cmdline}");
    let config = &launched["mcp_config"];
    let path = PathBuf::from(config["path"].as_str().unwrap());
    let turns = f.root.join(".branchyard/turns");
    assert!(path.starts_with(&turns), "{}", path.display());
    assert_eq!(config["mode"], "600");
    assert_eq!(config["servers"], serde_json::json!(["docs"]));
    // Gone with the turn; the secret is nowhere now.
    assert!(!path.exists());
    assert_eq!(fs::read_dir(&turns).unwrap().count(), 0);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(&turns).unwrap().permissions().mode() & 0o777,
        0o700
    );
    // The plain variable is stored with the request, as documented, and
    // is nowhere else.
    let mut found = containing(&f.dir, MCP_SECRET, Path::new("/nonexistent"));
    found.retain(|p| !p.to_string_lossy().contains("/.branchyard/state.db"));
    assert_eq!(found, Vec::<PathBuf>::new());
}

#[test]
fn requests_that_cannot_be_honored_are_refused_before_a_branch_exists() {
    let f = Fixture::new();
    // A secret for a harness that would run with your own home.
    let error = f
        .task("x")
        .options(TaskOptions {
            isolated: false,
            ..codex(&f, "BY_TEST_OPENAI_THREE", OPENAI_SECRET)
        })
        .run()
        .unwrap_err();
    assert!(error.to_string().contains("--isolated"), "{error}");
    assert!(!error.to_string().contains(OPENAI_SECRET));
    // Branchyard's own MCP server name.
    let error = f
        .task("x")
        .options(TaskOptions {
            provision: Some(Provisioning {
                mcp_servers: vec![McpServerSpec::parse("branchyard=/bin/sh").unwrap()],
                ..Provisioning::default()
            }),
            ..f.options()
        })
        .run()
        .unwrap_err();
    assert!(error.to_string().contains("Branchyard's own"), "{error}");
    assert!(f.yard.branches().unwrap().is_empty());
}

#[test]
fn a_fork_refused_for_its_provisioning_holds_no_name() {
    let f = Fixture::new();
    let parent = f.task("WRITE a.txt=1").name("parent").run().unwrap();
    // A secret for a fork that would run with your own home.
    let refused = TaskOptions {
        isolated: false,
        name: Some("kid".into()),
        ..codex(&f, "BY_TEST_OPENAI_FORK", OPENAI_SECRET)
    };
    let error = parent.fork("x", true, refused).unwrap_err();
    assert!(error.to_string().contains("--isolated"), "{error}");
    assert!(f.yard.branch("kid").is_err());
    // The name is free for a fork that can be honored.
    let kid = parent
        .fork(
            "WHOAMI",
            true,
            TaskOptions {
                name: Some("kid".into()),
                ..codex(&f, "BY_TEST_OPENAI_FORK", OPENAI_SECRET)
            },
        )
        .unwrap();
    assert_eq!(kid.info().name, "kid");
}

#[test]
fn a_secret_that_is_not_set_fails_the_turn_by_name() {
    let f = Fixture::new();
    let branch = f
        .task("x")
        .options(TaskOptions {
            harness: Some("codex-acp".into()),
            isolated: true,
            provision: Some(Provisioning {
                secrets: vec![SecretSource::parse("OPENAI_API_KEY=BY_TEST_NEVER_SET").unwrap()],
                ..Provisioning::default()
            }),
            ..f.options()
        })
        .name("unset")
        .run()
        .unwrap();
    assert!(
        matches!(&branch.info().status, BranchStatus::Failed { reason }
            if reason.contains("BY_TEST_NEVER_SET is not set")),
        "{:?}",
        branch.info().status
    );
}

#[test]
fn an_acp_harness_without_a_model_setting_refuses_one() {
    let f = Fixture::new();
    let branch = f
        .task("x")
        .options(TaskOptions {
            harness: Some("goose".into()),
            provision: Some(Provisioning {
                model: Some("m".into()),
                ..Provisioning::default()
            }),
            ..f.options()
        })
        .name("modelled")
        .run()
        .unwrap();
    assert!(
        matches!(&branch.info().status, BranchStatus::Failed { reason }
            if reason.contains("model selection over ACP")),
        "{:?}",
        branch.info().status
    );
}

#[test]
fn antigravity_accepts_mcp_servers_through_its_configuration() {
    // Its driver has no session channel for MCP servers and used to refuse
    // them; with a private home they go into its mcp_config.json. The
    // command is not Antigravity, so the turn fails after launch, not at
    // the driver.
    let f = Fixture::new();
    let branch = f
        .task("x")
        .options(TaskOptions {
            harness: Some("antigravity".into()),
            command: Some(vec!["/bin/sh".into(), "-c".into(), "exit 0".into()]),
            unapproved_tools: true,
            isolated: true,
            provision: Some(Provisioning {
                mcp_servers: vec![McpServerSpec::parse("docs=/usr/bin/docs-mcp").unwrap()],
                instructions: Some("Use the docs server.".into()),
                ..Provisioning::default()
            }),
            ..TaskOptions::default()
        })
        .name("agy")
        .run()
        .unwrap();
    let reason = match &branch.info().status {
        BranchStatus::Failed { reason } => reason.clone(),
        other => panic!("{other:?}"),
    };
    assert!(!reason.contains("cannot yet give"), "{reason}");
    let home = PathBuf::from(stored_record(&f.root, "agy")["home"].as_str().unwrap());
    let config = fs::read_to_string(home.join(".gemini/config/mcp_config.json")).unwrap();
    assert!(config.contains("/usr/bin/docs-mcp"), "{config}");
    let instructions = fs::read_to_string(home.join(".gemini/GEMINI.md")).unwrap();
    assert!(instructions.contains("Use the docs server."));

    // Without a private home it is refused, as before.
    let branch = f
        .task("x")
        .options(TaskOptions {
            harness: Some("antigravity".into()),
            command: Some(vec!["/bin/sh".into(), "-c".into(), "exit 0".into()]),
            unapproved_tools: true,
            provision: Some(Provisioning {
                mcp_servers: vec![McpServerSpec::parse("docs=/usr/bin/docs-mcp").unwrap()],
                ..Provisioning::default()
            }),
            ..TaskOptions::default()
        })
        .name("agy-local")
        .run()
        .unwrap();
    assert!(
        matches!(&branch.info().status, BranchStatus::Failed { reason } if reason.contains("--isolated")),
        "{:?}",
        branch.info().status
    );
}

/// Branches `owner`, provisioned with a Codex API key, and `sharer`, whose
/// record is changed to share `owner`'s private home, as a forked session
/// does. Returns the home.
fn shared_home(f: &Fixture, owner: &str, sharer: &str, var: &str) -> PathBuf {
    f.task("WRITE a.txt=1")
        .options(codex(f, var, OPENAI_SECRET))
        .name(owner)
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    f.task("WRITE b.txt=1")
        .options(TaskOptions {
            isolated: true,
            ..f.options()
        })
        .name(sharer)
        .run()
        .unwrap();
    let home = stored_record(&f.root, owner)["home"]
        .as_str()
        .unwrap()
        .to_owned();
    edit_record(&f.root, sharer, |r| r["home"] = serde_json::json!(home));
    let home = PathBuf::from(home);
    assert!(home.join(".codex/auth.json").is_file());
    home
}

#[test]
fn removing_a_branch_removes_its_credentials_even_from_a_home_a_fork_keeps() {
    let f = Fixture::new();
    let home = shared_home(&f, "owner", "sharer", "BY_TEST_OPENAI_RM");
    f.yard.remove("owner").unwrap();
    // The home stays for the branch sharing it, without the credential.
    assert!(home.join(".codex/config.toml").is_file());
    assert!(!home.join(".codex/auth.json").exists());
    assert_eq!(
        containing(&f.dir, OPENAI_SECRET, Path::new("/nonexistent")),
        Vec::<PathBuf>::new()
    );
    // Removing the last branch removes the home.
    f.yard.remove("sharer").unwrap();
    assert!(!home.exists());
}

#[test]
fn removal_can_keep_the_credentials_for_a_fork() {
    let f = Fixture::new();
    let home = shared_home(&f, "keeper", "kept", "BY_TEST_OPENAI_KEEP");
    f.yard
        .remove_with(
            "keeper",
            &RemoveOptions {
                keep_credentials: true,
            },
        )
        .unwrap();
    assert!(fs::read_to_string(home.join(".codex/auth.json"))
        .unwrap()
        .contains(OPENAI_SECRET));
    f.yard.remove("kept").unwrap();
    assert!(!home.exists());
}
