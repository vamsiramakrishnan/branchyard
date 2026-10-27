use super::*;

fn parse_str(line: &str) -> Result<Command, clap::Error> {
    let mut argv = vec!["by".to_owned()];
    argv.extend(split_words(line).unwrap());
    parse_from(argv).map(|cli| cli.command.expect("a command"))
}

fn err(line: &str) -> String {
    match parse_str(line) {
        Ok(command) => panic!("{line}: parsed as {command:?}"),
        Err(error) => error.to_string(),
    }
}

fn kind(line: &str) -> ErrorKind {
    parse_str(line).unwrap_err().kind()
}

fn task(line: &str) -> TaskArgs {
    match parse_str(line).unwrap() {
        Command::Run { task, .. } => task.into_inner(),
        Command::Fan { task, .. } => task.into_inner(),
        Command::Fork { task, .. } => task.into_inner(),
        Command::Reincarnate { task, .. } => task.into_inner(),
        Command::Send { task, .. } => task.into_inner(),
        Command::Review { task, .. } => task.into_inner(),
        other => panic!("{other:?} has no task"),
    }
}

#[test]
fn the_command_tree_is_consistent() {
    command().debug_assert();
}

/// `--no-preflight` skips the check made before a harness starts, on the
/// commands that make it (`crate::preflight`) and on no other.
#[test]
fn no_preflight_is_a_flag_of_the_commands_that_check_the_harness() {
    for line in ["run go", "fan go --harness a,b", "send b go"] {
        assert!(!task(line).no_preflight, "{line}");
        assert!(
            task(&format!("{line} --no-preflight")).no_preflight,
            "{line}"
        );
    }
    for line in ["fork b go", "map go", "reincarnate b"] {
        assert_eq!(
            kind(&format!("{line} --no-preflight")),
            ErrorKind::UnknownArgument
        );
    }
}

#[test]
fn map_runs_a_prompt_or_takes_an_action() {
    let Command::Map {
        action: None,
        map,
        json: false,
    } = parse_str(
        "map 'look at {{item.repo}}' --items r.csv --schema s.json --out o.csv \
         --concurrency 8 --retries 2 --total-usd 5 --reduce 'sum up' --rm -n look \
         --harness codex --budget-usd 1 --yes",
    )
    .unwrap()
    else {
        panic!("not a map run");
    };
    assert_eq!(map.prompt.as_deref(), Some("look at {{item.repo}}"));
    assert_eq!(map.items.as_deref(), Some("r.csv"));
    assert_eq!(map.concurrency, Some(8));
    assert_eq!(map.retries, Some(2));
    assert_eq!(map.total_usd, Some(5.0));
    assert!(map.rm);
    let task = map.task.clone().into_inner();
    assert_eq!(task.name.as_deref(), Some("look"));
    assert_eq!(task.harness.as_deref(), Some("codex"));
    assert_eq!(task.budget_usd, Some(1.0));
    assert_eq!(task.permissions, Permissions::Yes);
    assert!(matches!(
        parse_str("map resume look --retry-failed --json").unwrap(),
        Command::Map {
            action: Some(MapAction::Resume {
                retry_failed: true,
                ..
            }),
            json: true,
            ..
        }
    ));
    assert!(matches!(
        parse_str("map ls").unwrap(),
        Command::Map {
            action: Some(MapAction::Ls),
            ..
        }
    ));
    assert_eq!(kind("map"), ErrorKind::MissingRequiredArgument);
    assert_eq!(kind("map x --concurrency 0"), ErrorKind::ValueValidation);
    assert!(err("map x --items a --from-command b").contains("cannot be used with"));
    assert!(err("map x --reduce-out f").contains("--reduce"));
    assert!(err("map x --input-format xml").contains("not an item format"));
}

#[test]
fn run_takes_every_task_option() {
    let Command::Run { prompt, task, .. } = parse_str(
        "run 'fix the flaky test' --harness codex --name flaky --base main \
         --check 'cargo test -p core' --budget-usd 2.5 --max-turns 3 --yes \
         --max-minutes 1.5 --isolated --command '/opt/codex/bin/codex --flag'",
    )
    .unwrap() else {
        panic!("not run")
    };
    assert_eq!(prompt, "fix the flaky test");
    assert_eq!(
        *task,
        TaskArgs {
            harness: Some("codex".into()),
            name: Some("flaky".into()),
            base: Some("main".into()),
            check: Some(vec![
                "cargo".into(),
                "test".into(),
                "-p".into(),
                "core".into()
            ]),
            budget_usd: Some(2.5),
            max_turns: Some(3),
            max_duration: Some(Duration::from_secs(90)),
            stall_after: None,
            stall_action: branchyard::StallAction::Notify,
            permissions: Permissions::Yes,
            isolated: true,
            isolation_off: false,
            command: Some(vec!["/opt/codex/bin/codex".into(), "--flag".into()]),
            sandbox: None,
            substrate: None,
            recipe: None,
            local: false,
            delegate: None,
            allow_delegation: false,
            no_wake: false,
            unapproved_tools: false,
            deny: Vec::new(),
            provision: None,
            instructions: None,
            issue: None,
            pr: None,
            require_labels: Vec::new(),
            priority: None,
            auto: false,
            implied_auto: false,
            kind: None,
            seed: None,
            fleet: None,
            fleet_excluded: Vec::new(),
            plan: false,
            goal: None,
            goal_rounds: None,
            goal_judge: None,
            goal_judge_command: None,
            no_preflight: false,
        }
    );
}

/// `--isolated` alone means yes; `--isolated=false` says no, and says
/// it was said, so `[defaults] isolated = true` does not win.
#[test]
fn isolated_takes_an_optional_value() {
    let isolated = |line: &str| {
        let task = task(line);
        (task.isolated, task.isolation_off)
    };
    assert_eq!(isolated("run go"), (false, false));
    assert_eq!(isolated("run go --isolated"), (true, false));
    assert_eq!(isolated("run go --isolated=true"), (true, false));
    assert_eq!(isolated("run go --isolated=yes"), (true, false));
    assert_eq!(isolated("run go --isolated=false"), (false, true));
    assert_eq!(isolated("run go --isolated=no"), (false, true));
    // Without the `=`, the next word is still the prompt.
    assert_eq!(isolated("run --isolated go"), (true, false));
    assert!(err("run go --isolated=maybe").contains("--isolated"));
}

#[test]
fn short_flags_and_value_formats() {
    let short = task("run go -n flaky -b main -y --budget-usd $1.25 --max-minutes 90s");
    assert_eq!(short.name.as_deref(), Some("flaky"));
    assert_eq!(short.base.as_deref(), Some("main"));
    assert_eq!(short.permissions, Permissions::Yes);
    assert_eq!(short.budget_usd, Some(1.25));
    assert_eq!(short.max_duration, Some(Duration::from_secs(90)));
    let stall = task("run go --stall-after 2h --stall-action interrupt");
    assert_eq!(stall.stall_after, Some(Duration::from_secs(7200)));
    assert_eq!(stall.stall_action, branchyard::StallAction::Interrupt);
    assert!(err("run go --stall-action interrupt").contains("--stall-after"));
    assert!(err("run go --stall-after 1 --stall-action later").contains("[possible values"));
    assert!(err("run go --max-minutes 5d").contains("positive number of minutes"));
}

#[test]
fn provisioning_flags_repeat_and_parse() {
    let task = task(
        "run go --isolated --secret ANTHROPIC_API_KEY --secret CODEX_AUTH=@/run/auth.json \
         --secret OPENAI_API_KEY=MY_KEY --mcp 'docs=/usr/bin/docs-mcp --stdio' \
         --auth api-key --model large --effort 80 --telemetry http://127.0.0.1:4317 \
         --instructions rules.md",
    );
    let spec = task.provision.unwrap();
    assert_eq!(spec.secrets.len(), 3);
    assert_eq!(
        spec.secrets[1].from,
        Some(branchyard::SecretFrom::File {
            path: "/run/auth.json".into()
        })
    );
    assert_eq!(spec.mcp_servers[0].args, ["--stdio"]);
    assert_eq!(spec.auth.as_deref(), Some("api-key"));
    assert_eq!(spec.effort, Some(branchyard::Effort::Xhigh));
    assert_eq!(spec.telemetry.unwrap().endpoint(), "http://127.0.0.1:4317");
    assert_eq!(task.instructions.as_deref(), Some("rules.md"));
    assert!(self::task("send b go").provision.is_none());
    assert!(self::task("send b go --instructions r.md")
        .provision
        .is_some());
    for (line, error) in [
        ("run go --mcp docs=relative", "absolute command"),
        ("run go --secret 1BAD", "--secret"),
        ("run go --effort max", "--effort"),
        ("run go --telemetry collector:4317", "--telemetry"),
        ("run go --model ' '", "--model"),
    ] {
        assert!(err(line).contains(error), "{line}: {}", err(line));
    }
    assert_eq!(
        kind("run go --model a --model b"),
        ErrorKind::ArgumentConflict
    );
    assert!(err("run go --model a --model b").contains("cannot be used multiple times"));
}

#[test]
fn the_model_gateway_flag_parses() {
    for line in [
        "run go --model-gateway",
        "fan go --harness a,b --model-gateway",
        "fork b go --model-gateway",
        "send b go --model-gateway",
    ] {
        let spec = task(line).provision.unwrap();
        assert_eq!(
            spec.models,
            Some(branchyard::models::ModelAccess::all()),
            "{line}"
        );
    }
    let spec = task("run go --model-gateway='claude-*,gpt-5'")
        .provision
        .unwrap();
    assert_eq!(spec.models.unwrap().allow, ["claude-*", "gpt-5"]);
    assert_eq!(task("run go").provision, None);
    assert!(err("run go --model-gateway='a b'").contains("' '"));
    assert_eq!(
        parse_str("models --period day --json").unwrap(),
        Command::Models {
            period: "day".into(),
            json: true,
            action: None
        }
    );
    assert!(err("models --period week").contains("week"));
    // `list` is a subcommand, and --json goes after it.
    let Command::Models { json, action, .. } = parse_str("models list --json").unwrap() else {
        panic!("models list --json");
    };
    assert!(json);
    assert_eq!(
        action,
        Some(ModelsAction::List {
            refresh: false,
            harness: None,
            gemini_url: "https://generativelanguage.googleapis.com".into(),
            openai_url: "https://api.openai.com".into(),
            anthropic_url: "https://api.anthropic.com".into(),
        })
    );
    let Command::Models { action, .. } =
        parse_str("models list --refresh --harness codex --openai-url http://127.0.0.1:9").unwrap()
    else {
        panic!("models list");
    };
    let Some(ModelsAction::List {
        refresh,
        harness,
        openai_url,
        ..
    }) = action
    else {
        panic!("models list");
    };
    assert!(refresh);
    assert_eq!(harness.as_deref(), Some("codex"));
    assert_eq!(openai_url, "http://127.0.0.1:9");
    // The usage view's flag does not go with a subcommand.
    assert!(parse_str("models --period day list").is_err());
}

#[test]
fn network_and_permission_preset_flags_parse() {
    for line in [
        "run go --network 'github.com,*.npmjs.org:443' --network-enforce required",
        "fan go --harness a,b --network 'github.com,*.npmjs.org:443' --network-enforce required",
        "fork b go --network 'github.com,*.npmjs.org:443' --network-enforce required",
        "send b go --network 'github.com,*.npmjs.org:443' --network-enforce required",
    ] {
        let spec = task(line).provision.unwrap_or_else(|| panic!("{line}"));
        assert_eq!(
            spec.network.unwrap().to_string(),
            "github.com, *.npmjs.org:443 (required)",
            "{line}"
        );
    }
    let none = task("run go --network none").provision.unwrap();
    assert_eq!(none.network, Some(branchyard::Network::none()));
    for (line, error) in [
        ("run go --network https://x.com", "not a URL"),
        ("run go --network '*'", "\"open\""),
        ("run go --network-enforce required", "--network"),
        (
            "run go --network none --network-enforce always",
            "best_effort",
        ),
        (
            "run go --permissions yolo",
            "read-only, edit-worktree, full",
        ),
        ("run go --permissions full --yes", "cannot be used with"),
    ] {
        assert!(err(line).contains(error), "{line}: {}", err(line));
    }
    for (line, preset) in [
        (
            "run go --permissions read-only",
            branchyard::PolicyPreset::ReadOnly,
        ),
        (
            "send b go --permissions edit-worktree",
            branchyard::PolicyPreset::EditWorktree,
        ),
        (
            "fan go --harness a,b --permissions full",
            branchyard::PolicyPreset::Full,
        ),
    ] {
        assert_eq!(
            task(line).permissions,
            Permissions::Preset(preset),
            "{line}"
        );
    }
}

#[test]
fn connector_flags_repeat_and_parse() {
    for line in [
        "run go --isolated --connector github --connector 'github:write+confirm:issues.create'",
        "fan go --harness a,b --isolated --connector github --connector 'github:write+confirm:issues.create'",
        "fork b go --connector github --connector 'github:write+confirm:issues.create'",
        "send b go --connector github --connector 'github:write+confirm:issues.create'",
    ] {
        let spec = task(line).provision.unwrap_or_else(|| panic!("{line}"));
        let grants: Vec<String> = spec.connectors.iter().map(|g| g.to_string()).collect();
        assert_eq!(
            grants,
            ["github:read", "github:write+confirm:issues.create"],
            "{line}"
        );
    }
    for (line, error) in [
        ("run go --connector github:admin", "mode"),
        ("run go --connector 'a b'", "connector id"),
        ("run go --connector github:read:", "operation"),
    ] {
        assert!(err(line).contains(error), "{line}: {}", err(line));
    }
    assert!(matches!(
        parse_str("gateway status --json").unwrap(),
        Command::Gateway {
            json: true,
            action: GatewayAction::Status
        }
    ));
    assert!(matches!(
        parse_str("gateway rotate-key --keep 2").unwrap(),
        Command::Gateway {
            action: GatewayAction::RotateKey { keep: 2 },
            ..
        }
    ));
    assert_eq!(
        parse_str("connect github --account work --api-key-stdin").unwrap(),
        Command::Connect {
            connector: "github".into(),
            account: Some("work".into()),
            api_key_stdin: true,
            open: false,
        }
    );
}

#[test]
fn substrate_flags_configure_tls_and_the_insecure_escape() {
    let task = task(
        "run go --provider substrate --substrate-endpoint https://control:443 \
         --substrate-router 'wss://router/{atespace}/{actor}/' --substrate-template t \
         --substrate-key k --substrate-ca ca.pem --substrate-client-cert c.pem \
         --substrate-client-key c.key --substrate-router-ca router-ca.pem",
    );
    let substrate = task.substrate.unwrap();
    assert_eq!(substrate.endpoint, "https://control:443");
    assert_eq!(substrate.ca.as_deref(), Some("ca.pem"));
    assert_eq!(substrate.client_cert.as_deref(), Some("c.pem"));
    assert_eq!(substrate.client_key.as_deref(), Some("c.key"));
    assert_eq!(substrate.router_ca.as_deref(), Some("router-ca.pem"));
    assert!(!substrate.insecure);
    let task = self::task(
        "fork b go --provider substrate --substrate-endpoint http://10.0.0.1:8080 \
         --substrate-router 'http://10.0.0.2/{actor}/' --substrate-template t \
         --substrate-key k --substrate-insecure",
    );
    let substrate = task.substrate.unwrap();
    assert!(substrate.insecure && substrate.ca.is_none());
    assert!(err("run go --substrate-insecure")
        .contains("--substrate-insecure needs --provider substrate"));
    assert!(err("run go --provider local --substrate-ca ca.pem")
        .contains("--substrate-ca needs --provider substrate"));
    assert!(err("run go --provider substrate --substrate-endpoint e")
        .contains("--provider substrate needs --substrate-router"));
    assert_eq!(
        kind("run go --substrate-insecure"),
        ErrorKind::ArgumentConflict
    );
}

#[test]
fn provider_flags_select_and_configure_a_sandbox() {
    let task = task(
        "run go --provider microsandbox --image ghcr.io/x/claude:1 --cpus 2 \
         --memory 4096 --pass-env 'ANTHROPIC_API_KEY, GH_TOKEN'",
    );
    assert_eq!(
        task.sandbox,
        Some(SandboxArgs {
            image: "ghcr.io/x/claude:1".into(),
            cpus: Some(2),
            memory_mib: Some(4096),
            pass_env: vec!["ANTHROPIC_API_KEY".into(), "GH_TOKEN".into()],
            ..SandboxArgs::default()
        })
    );
    let kept = self::task(
        "run go --provider microsandbox --image a --live-branch --keep-sandbox pause \
         --sandbox-snapshots 2 --max-paused 6 --max-kept-minutes 90",
    );
    assert_eq!(
        kept.sandbox,
        Some(SandboxArgs {
            image: "a".into(),
            lifecycle: LifecycleArgs {
                keep: Some(branchyard::SandboxKeep::Pause),
                snapshots: Some(2),
                max_paused: Some(6),
                max_kept_minutes: Some(90),
            },
            live_branch: true,
            ..SandboxArgs::default()
        })
    );
    assert!(err("run go --keep-sandbox pause")
        .contains("--keep-sandbox needs --provider microsandbox, substrate or recipe:NAME"));
    assert!(
        err("run go --provider microsandbox --image a --keep-sandbox forever")
            .contains("not pause or destroy")
    );
    assert!(err("run go --live-branch").contains("--live-branch needs --provider microsandbox"));
    assert!(
        err("run go --provider microsandbox --image a --max-paused 0").contains("--max-paused")
    );
    assert!(!task.local);
    let task = self::task("fork b go --provider local");
    assert!(task.local && task.sandbox.is_none());
    assert!(err("run go --provider microsandbox").contains("--provider microsandbox needs --image"));
    assert!(err("run go --image alpine").contains("--image needs --provider microsandbox"));
    assert!(
        err("run go --provider local --cpus 2").contains("--cpus needs --provider microsandbox")
    );
    assert!(err("run go --provider local --pass-env A")
        .contains("--pass-env needs --provider microsandbox, substrate or recipe:NAME"));
    let docker = err("run go --provider docker");
    assert!(
        docker.contains("[possible values: local, microsandbox, substrate, recipe:NAME]"),
        "{docker}"
    );
    assert!(err("run go --provider microsandbox --image a --cpus 0").contains("--cpus"));
    assert!(err("run go --provider microsandbox --image a --memory 1g").contains("--memory"));
    assert!(err("run go --provider microsandbox --image a --pass-env A=1").contains("names"));
    assert_eq!(
        kind("send b go --provider local"),
        ErrorKind::UnknownArgument
    );
}

#[test]
fn provider_recipe_names_a_recipe_and_its_paths() {
    let task = self::task(
        "run go --provider recipe:devbox --recipe-workdir /srv/work --pass-env TOKEN \
         --keep-sandbox pause --max-paused 2",
    );
    assert_eq!(
        task.recipe,
        Some(RecipeArgs {
            name: "devbox".into(),
            workdir: Some("/srv/work".into()),
            home: None,
            pass_env: vec!["TOKEN".into()],
            lifecycle: LifecycleArgs {
                keep: Some(branchyard::SandboxKeep::Pause),
                snapshots: None,
                max_paused: Some(2),
                max_kept_minutes: None,
            },
        })
    );
    assert!(task.sandbox.is_none() && task.substrate.is_none() && !task.local);
    let fan = self::task("fan go --harness codex,claude --provider recipe:lab.box");
    assert_eq!(fan.recipe.map(|r| r.name), Some("lab.box".into()));
    assert!(err("run go --provider recipe:").contains("not a recipe name"));
    assert!(err("run go --provider recipe:Dev").contains("not a recipe name"));
    assert!(err("run go --recipe-workdir /w").contains("needs --provider recipe:NAME"));
    assert!(err("run go --provider recipe:devbox --recipe-home home").contains("must be absolute"));
    assert!(err("run go --provider recipe:devbox --sandbox-snapshots 2").contains("no snapshots"));
    assert!(err("run go --provider recipe:devbox --image a")
        .contains("--image needs --provider microsandbox"));
}

#[test]
fn delegate_takes_an_optional_inline_depth() {
    assert_eq!(task("run go").delegate, None);
    assert_eq!(task("run go --delegate").delegate, Some(1));
    let Command::Run { prompt, task, .. } = parse_str("run --delegate go").unwrap() else {
        panic!("not run")
    };
    assert_eq!(prompt, "go", "the prompt is not a depth");
    assert_eq!(task.delegate, Some(1));
    assert_eq!(self::task("run go --delegate=3").delegate, Some(3));
    assert_eq!(self::task("send b go --delegate=2").delegate, Some(2));
    assert!(err("run go --delegate=0").contains("positive whole number"));
    assert!(err("run go --delegate=x").contains("positive whole number"));
    let help = help("run");
    assert!(help.contains("--delegate[=<DEPTH>]"), "{help}");
}

#[test]
fn delegation_commands_parse_with_optional_branches() {
    let Command::Spawn { prompt, spawn, .. } = parse_str(
        "spawn 'fix it' --parent root --harness codex --name fix --budget-usd 0.5 \
         --max-depth 0 --deny Bash,mcp__* --wait --json --yes",
    )
    .unwrap() else {
        panic!("not spawn")
    };
    assert_eq!(prompt, "fix it");
    assert_eq!(spawn.parent.as_deref(), Some("root"));
    assert_eq!(spawn.task.harness.as_deref(), Some("codex"));
    assert_eq!(spawn.task.name.as_deref(), Some("fix"));
    assert_eq!(spawn.task.budget_usd, Some(0.5));
    assert_eq!(spawn.task.permissions, Permissions::Yes);
    assert_eq!(spawn.max_depth, Some(0));
    assert_eq!(spawn.deny, ["Bash", "mcp__*"]);
    assert!(spawn.wait && spawn.json);
    let Command::Spawn { spawn, .. } = parse_str(
        "spawn go --parent p --depends-on a,b --after integrated --bind cache:read_only \
         --bind out:exclusive_write --seat worker",
    )
    .unwrap() else {
        panic!("not spawn")
    };
    assert_eq!(spawn.depends_on, ["a", "b"]);
    assert_eq!(spawn.after, branchyard::After::Integrated);
    assert_eq!(spawn.bindings.len(), 2);
    assert!(spawn.connectors.is_empty());
    assert_eq!(spawn.model, None);
    let Command::Spawn { spawn: cheap, .. } = parse_str("spawn go --model haiku").unwrap() else {
        panic!("not spawn")
    };
    assert_eq!(cheap.model.as_deref(), Some("haiku"));
    assert!(err("spawn go --model ' '").contains("--model"));
    let Command::Spawn { spawn: granted, .. } =
        parse_str("spawn go --connector github:read --connector 'linear@work:write:issues.*'")
            .unwrap()
    else {
        panic!("not spawn")
    };
    let grants: Vec<String> = granted.connectors.iter().map(|g| g.to_string()).collect();
    assert_eq!(grants, ["github:read", "linear@work:write:issues.*"]);
    let Command::Spawn { spawn: owner, .. } =
        parse_str("spawn go --owns src/parser,tests/parser_*.rs --owns docs/parser.md").unwrap()
    else {
        panic!("not spawn")
    };
    assert_eq!(
        owner.owns,
        ["src/parser", "tests/parser_*.rs", "docs/parser.md"]
    );
    assert!(granted.owns.is_empty());
    assert!(!owner.land);
    let Command::Spawn { spawn: lander, .. } = parse_str("spawn go --land").unwrap() else {
        panic!("not spawn")
    };
    assert!(lander.land);
    assert!(lander.review.is_none());
    let Command::Spawn { spawn: checked, .. } = parse_str(
        "spawn go --land --review codex --review-model large --review-budget-usd 0.5 \
         --review-rubric 'No new crates'",
    )
    .unwrap() else {
        panic!("not spawn")
    };
    assert_eq!(
        checked.review.as_deref(),
        Some(&branchyard::ReviewSpec {
            harness: "codex".into(),
            model: Some("large".into()),
            rubric: Some("No new crates".into()),
            budget_usd: Some(0.5),
        })
    );
    assert!(err("spawn go --review codex").contains("--land"));
    assert!(err("spawn go --land --review-model large").contains("--review"));
    let Command::Delegate { land, .. } = parse_str("delegate --plan p.json --land").unwrap() else {
        panic!("not delegate")
    };
    assert!(land);
    assert_eq!(spawn.seat.as_deref(), Some("worker"));
    assert!(err("spawn go --after soon").contains("[possible values: settled, integrated]"));
    assert!(err("spawn go --bind cache").contains("--bind"));
    assert!(err("spawn go --depends-on a,a").contains("lists a twice"));
    assert_eq!(
        parse_str("inspect").unwrap(),
        Command::Inspect {
            branch: None,
            json: false
        }
    );
    assert_eq!(
        parse_str("inspect kid --json").unwrap(),
        Command::Inspect {
            branch: Some("kid".into()),
            json: true
        }
    );
    assert_eq!(
        parse_str("events kid --cursor 7 --limit 3").unwrap(),
        Command::Events {
            branch: Some("kid".into()),
            cursor: Some(7),
            limit: Some(3),
            json: false
        }
    );
    assert_eq!(
        parse_str("children").unwrap(),
        Command::Children {
            branch: None,
            json: false
        }
    );
    assert_eq!(
        parse_str("check").unwrap(),
        Command::Check {
            branch: None,
            json: false
        }
    );
    assert_eq!(
        parse_str("check kid --json").unwrap(),
        Command::Check {
            branch: Some("kid".into()),
            json: true
        }
    );
    assert_eq!(
        parse_str("integrate kid --json").unwrap(),
        Command::Integrate {
            branches: vec!["kid".into()],
            json: true
        }
    );
    assert_eq!(
        parse_str("integrate a b").unwrap(),
        Command::Integrate {
            branches: vec!["a".into(), "b".into()],
            json: false
        }
    );
    assert_eq!(
        parse_str("wait a b --any --timeout 2.5 --json").unwrap(),
        Command::Wait {
            branches: vec!["a".into(), "b".into()],
            any: true,
            all: false,
            timeout: Some(2.5),
            json: true
        }
    );
    assert!(err("wait --any --all").contains("cannot be used with"));
    assert_eq!(
        parse_str("cancel kid").unwrap(),
        Command::Cancel {
            branch: "kid".into(),
            json: false
        }
    );
    assert_eq!(kind("integrate"), ErrorKind::MissingRequiredArgument);
    assert!(err("integrate").contains("<BRANCH>"));
    assert!(err("inspect a b").contains("unexpected argument 'b'"));
    assert!(err("events --cursor x").contains("invalid digit"));
    assert!(help("inspect").contains("Usage: by inspect [OPTIONS] [BRANCH]"));
    assert!(task("run go --delegate --allow-delegation").allow_delegation);
    assert!(task("send b go --allow-unapproved-tools").unapproved_tools);
    for line in [
        "run go --require-label gpu --require-label linux",
        "fan go --harness a,b --require-label gpu --require-label linux",
        "send b go --require-label gpu --require-label linux",
        "fork b go --require-label gpu --require-label linux",
        "reincarnate b --require-label gpu --require-label linux",
    ] {
        assert_eq!(task(line).require_labels, ["gpu", "linux"], "{line}");
    }
}

#[test]
fn messages_take_text_and_an_acting_branch() {
    assert_eq!(
        parse_str("ask 'which lock?' --as kid --wait 2.5 --json").unwrap(),
        Command::Ask {
            as_branch: Some("kid".into()),
            text: "which lock?".into(),
            wait_seconds: Some(2.5),
            json: true
        }
    );
    assert_eq!(
        parse_str("answer 7 'the mutex'").unwrap(),
        Command::Answer {
            as_branch: None,
            message_id: 7,
            text: "the mutex".into(),
            json: false
        }
    );
    assert_eq!(
        parse_str("inbox --unread").unwrap(),
        Command::Inbox {
            as_branch: None,
            unread: true,
            json: false
        }
    );
    assert!(err("answer x hi").contains("<MESSAGE_ID>"));
    assert!(err("ask hi --wait -1").contains("needs a number of seconds"));
}

#[test]
fn send_steer_is_a_switch() {
    let Command::Send {
        steer, json, task, ..
    } = parse_str("send b --steer --json also-this").unwrap()
    else {
        panic!("not send")
    };
    assert!(steer && json);
    assert_eq!(*task, TaskArgs::default());
    let Command::Send { steer, .. } = parse_str("send b go").unwrap() else {
        panic!("not send")
    };
    assert!(!steer);
}

#[test]
fn send_and_steer_take_a_prompt_file() {
    let Command::Send {
        steer, prompt_file, ..
    } = parse_str("send b --steer --prompt-file note.md").unwrap()
    else {
        panic!("not send")
    };
    assert!(steer);
    assert_eq!(prompt_file.as_deref(), Some("note.md"));
    let Command::Send { prompt_file, .. } = parse_str("send b --prompt-file -").unwrap() else {
        panic!("not send")
    };
    assert_eq!(prompt_file.as_deref(), Some("-"));
    assert!(err("send b go --prompt-file f").contains("cannot be used with"));
    assert!(err("send b --retry --prompt-file f").contains("cannot be used with"));
    assert_eq!(
        parse_str("steer b also --json").unwrap(),
        Command::Steer {
            branch: "b".into(),
            prompt: "also".into(),
            prompt_file: None,
            json: true
        }
    );
    let Command::Steer { prompt_file, .. } = parse_str("steer b --prompt-file f").unwrap() else {
        panic!("not steer")
    };
    assert_eq!(prompt_file.as_deref(), Some("f"));
    assert_eq!(kind("steer b"), ErrorKind::MissingRequiredArgument);
}

#[test]
fn mcp_needs_a_root_and_a_branch() {
    assert_eq!(
        parse_str("mcp --root /r --branch b").unwrap(),
        Command::Mcp {
            root: Some("/r".into()),
            branch: Some("b".into()),
            list_tools: false,
        }
    );
    assert_eq!(kind("mcp --root /r"), ErrorKind::MissingRequiredArgument);
    assert!(err("mcp --root /r").contains("--branch <NAME>"));
    // --list-tools stands alone: no repository, branch or token.
    assert_eq!(
        parse_str("mcp --list-tools").unwrap(),
        Command::Mcp {
            root: None,
            branch: None,
            list_tools: true,
        }
    );
    assert_eq!(
        kind("mcp --list-tools --root /r"),
        ErrorKind::ArgumentConflict
    );
}

/// `--deny` takes a comma list and repeats, on every command that has
/// it; a repeat failed with "cannot be used multiple times".
#[test]
fn deny_repeats_as_well_as_a_comma_list() {
    let Command::Spawn { spawn, .. } =
        parse_str("spawn go --parent p --deny Edit --deny Write,Bash").unwrap()
    else {
        panic!("not spawn")
    };
    assert_eq!(spawn.deny, ["Edit", "Write", "Bash"]);
    assert_eq!(
        task("run go --deny Edit --deny Write,Bash").deny,
        ["Edit", "Write", "Bash"]
    );
    assert_eq!(task("run go --deny Edit,Write").deny, ["Edit", "Write"]);
}

#[test]
fn run_defaults_and_equals_form() {
    let Command::Run { prompt, task, .. } = parse_str("run --ask --name=x \"do it\"").unwrap()
    else {
        panic!("not run")
    };
    assert_eq!(prompt, "do it");
    assert_eq!(task.name.as_deref(), Some("x"));
    assert_eq!(task.permissions, Permissions::Ask);
    assert_eq!(self::task("run go"), TaskArgs::default());
}

#[test]
fn fan_takes_a_harness_list_or_routes() {
    let Command::Fan {
        harnesses, task, ..
    } = parse_str("fan go --harness 'claude-code, codex' --max-turns 2").unwrap()
    else {
        panic!("not fan")
    };
    assert_eq!(*harnesses.unwrap(), ["claude-code", "codex"]);
    assert_eq!(task.harness, None);
    assert_eq!(task.max_turns, Some(2));
    // Without --harness it routes through [fleet]; the command says so
    // when there is none.
    let Command::Fan {
        harnesses,
        task,
        attempts,
        judge,
        ..
    } = parse_str("fan go --auto --kind bugfix --attempts 3 --judge --seed 9").unwrap()
    else {
        panic!("not fan")
    };
    assert_eq!(harnesses, None);
    assert!(task.auto && judge);
    assert_eq!(
        (task.kind, task.seed, attempts),
        (Some(branchyard::TaskKind::Bugfix), Some(9), Some(3))
    );
    assert!(err("fan go --attempts 0").contains("0"));
    assert!(err("fan go --harness codex,codex").contains("lists codex twice"));
    assert!(err("fan go --harness codex,").contains("empty entry"));
}

#[test]
fn send_and_fork_take_a_branch_and_a_prompt() {
    let Command::Send {
        branch,
        prompt,
        prompt_file,
        task,
        steer,
        retry,
        wait,
        json,
        detach,
    } = parse_str("send flaky 'now add a test' --yes").unwrap()
    else {
        panic!("not send")
    };
    assert_eq!(
        (branch.as_str(), prompt.as_str()),
        ("flaky", "now add a test")
    );
    assert_eq!(
        *task,
        TaskArgs {
            permissions: Permissions::Yes,
            ..TaskArgs::default()
        }
    );
    assert!(!steer && !retry && !wait && !json && !detach && prompt_file.is_none());
    // --retry takes the cut-off turn's prompt, so none is given with it.
    let Command::Send { retry, prompt, .. } = parse_str("send flaky --retry").unwrap() else {
        panic!("not send")
    };
    assert!(retry && prompt.is_empty());
    assert!(parse_str("send flaky").is_err());
    assert!(parse_str("send flaky go --retry").is_err());
    assert!(parse_str("send flaky --retry --steer").is_err());
    let Command::Fork {
        branch,
        prompt,
        fresh_session,
        at: None,
        task,
        ..
    } = parse_str("fork flaky 'try another way' --fresh-session --name alt").unwrap()
    else {
        panic!("not fork")
    };
    assert_eq!(
        (branch.as_str(), prompt.as_str()),
        ("flaky", "try another way")
    );
    assert!(fresh_session);
    assert_eq!(
        *task,
        TaskArgs {
            name: Some("alt".into()),
            ..TaskArgs::default()
        }
    );
    assert!(err("send flaky").contains("<PROMPT>"));
    assert!(err("fork").contains("<BRANCH>"));
    assert!(err("send flaky go --harness codex").contains("unexpected argument '--harness'"));
}

#[test]
fn detach_refuses_what_needs_a_terminal_or_a_wait() {
    let Command::Run { detached, .. } = parse_str("run go --detach --json --yes").unwrap() else {
        panic!("not run")
    };
    assert_eq!(
        detached,
        Detached {
            detach: true,
            json: true
        }
    );
    for line in [
        "run go --detach --ask",
        "fork b go --detach --ask",
        "send b go --detach --ask",
        "send b go --detach --steer",
        "send b go --detach --wait",
    ] {
        assert_eq!(kind(line), ErrorKind::ArgumentConflict, "{line}");
    }
    // --json on run and fork needs no --detach: the foreground run prints
    // the branch as `show --json` does (`run_and_fork_take_json_without_detach`).
    assert!(parse_str("run go --json").is_ok());
    assert!(parse_str("fork b go --json").is_ok());
}

#[test]
fn checkpoint_try_and_compare_commands() {
    assert_eq!(
        parse_str("rewind b --to 2 -y").unwrap(),
        Command::Rewind {
            branch: "b".into(),
            to: 2,
            yes: true,
            json: false
        }
    );
    assert!(err("rewind b").contains("--to <N>"));
    let Command::Fork { at, .. } = parse_str("fork b next --at 0").unwrap() else {
        panic!("not fork")
    };
    assert_eq!(at, Some(0));
    assert_eq!(
        kind("fork b next --at 1 --fresh-session"),
        ErrorKind::ArgumentConflict
    );
    assert!(matches!(
        parse_str("try --off --force").unwrap(),
        Command::Try {
            branch: None,
            off: true,
            force: true,
            ..
        }
    ));
    assert!(matches!(
        parse_str("try b").unwrap(),
        Command::Try {
            branch: Some(_),
            off: false,
            status: false,
            ..
        }
    ));
    assert_eq!(kind("try"), ErrorKind::MissingRequiredArgument);
    assert_eq!(kind("try b --off"), ErrorKind::ArgumentConflict);
    assert_eq!(kind("try b --force"), ErrorKind::ArgumentConflict);
    assert_eq!(kind("try --force"), ErrorKind::MissingRequiredArgument);
    let Command::Compare {
        branches,
        fan,
        diff,
        pick,
        discard_others,
        ..
    } = parse_str("compare --fan speed --pick speed-codex --discard-others").unwrap()
    else {
        panic!("not compare")
    };
    assert!(branches.is_empty() && discard_others);
    assert_eq!(
        (fan.as_deref(), pick.as_deref(), diff),
        (Some("speed"), Some("speed-codex"), None)
    );
    assert!(matches!(
        parse_str("compare --diff a b").unwrap(),
        Command::Compare { diff: Some(pair), .. } if pair == ["a", "b"]
    ));
    assert_eq!(kind("compare"), ErrorKind::MissingRequiredArgument);
    assert_eq!(kind("compare a --fan x"), ErrorKind::ArgumentConflict);
    assert_eq!(
        kind("compare a --discard-others"),
        ErrorKind::MissingRequiredArgument
    );
}

#[test]
fn inspection_commands() {
    assert_eq!(parse_str("ls").unwrap(), Command::Ls { json: false });
    assert_eq!(parse_str("ls --json").unwrap(), Command::Ls { json: true });
    assert_eq!(
        parse_str("show b --json").unwrap(),
        Command::Show {
            branch: "b".into(),
            json: true,
            refresh: false
        }
    );
    assert_eq!(
        parse_str("diff b").unwrap(),
        Command::Diff { branch: "b".into() }
    );
    assert_eq!(
        parse_str("log b").unwrap(),
        Command::Log {
            branch: "b".into(),
            json: false,
            follow: false
        }
    );
    assert_eq!(
        parse_str("log -f b").unwrap(),
        Command::Log {
            branch: "b".into(),
            json: false,
            follow: true
        }
    );
    let harnesses = |json, all| Command::Harnesses {
        json,
        all,
        profiles: false,
        refresh: false,
        on: None,
        action: None,
    };
    assert_eq!(
        parse_str("harnesses --json").unwrap(),
        harnesses(true, false)
    );
    assert_eq!(
        parse_str("harnesses --all").unwrap(),
        harnesses(false, true)
    );
    assert_eq!(
        parse_str("harnesses install codex --on ssh://h --version 1.2 --yes --json").unwrap(),
        Command::Harnesses {
            json: true,
            all: false,
            profiles: false,
            refresh: false,
            on: Some("ssh://h".into()),
            action: Some(HarnessesAction::Install(HarnessChange {
                id: "codex".into(),
                version: Some("1.2".into()),
                yes: true,
            })),
        }
    );
    assert_eq!(
        parse_str("harnesses login codex --api-key").unwrap(),
        Command::Harnesses {
            json: false,
            all: false,
            profiles: false,
            refresh: false,
            on: None,
            action: Some(HarnessesAction::Login {
                id: "codex".into(),
                api_key: true,
                provider: None,
                yes: false,
            }),
        }
    );
    assert!(parse_str("harnesses --all --on ssh://h").is_err());
    assert_eq!(
        parse_str("connectors --json catalog").unwrap(),
        Command::Connectors {
            json: true,
            action: ConnectorsAction::Catalog
        }
    );
    assert_eq!(
        parse_str("connectors catalog --json").unwrap(),
        Command::Connectors {
            json: true,
            action: ConnectorsAction::Catalog
        }
    );
    assert_eq!(
        parse_str("services --kind connector_gateway --json").unwrap(),
        Command::Services {
            json: true,
            kind: Some("connector_gateway".into()),
            all: false,
            action: None,
        }
    );
    assert_eq!(
        parse_str("sync fix-login --json").unwrap(),
        Command::Sync(SyncArgs {
            json: true,
            task: Some("fix-login".into()),
            action: None,
        })
    );
    assert_eq!(
        parse_str("sync gc --dry-run").unwrap(),
        Command::Sync(SyncArgs {
            json: false,
            task: None,
            action: Some(SyncAction::Gc { dry_run: true }),
        })
    );
    assert_eq!(
        parse_str("sync hold t --reason audit --release").unwrap(),
        Command::Sync(SyncArgs {
            json: false,
            task: None,
            action: Some(SyncAction::Hold {
                task: "t".into(),
                reason: Some("audit".into()),
                release: true,
            }),
        })
    );
    assert_eq!(
        parse_str("services gc --json").unwrap(),
        Command::Services {
            json: true,
            kind: None,
            all: false,
            action: Some(ServicesAction::Gc),
        }
    );
    assert!(matches!(
        parse_str("catalog refresh --only harnesses --npm-registry http://127.0.0.1:1")
            .unwrap(),
        Command::Catalog {
            action: CatalogAction::Refresh { ref only, ref npm_registry, max_pages: 20, .. },
            ..
        } if only.as_deref() == Some("harnesses") && npm_registry == "http://127.0.0.1:1"
    ));
    assert!(err("catalog refresh --only models").contains("invalid value"));
    assert!(err("diff b --json").contains("unexpected argument '--json'"));
}

#[test]
fn merge_and_rm() {
    assert_eq!(
        parse_str("merge b").unwrap(),
        Command::Merge {
            branch: "b".into(),
            into: None,
            rm: false,
            promote_effects: false,
            json: false
        }
    );
    assert_eq!(
        parse_str("merge b --into release").unwrap(),
        Command::Merge {
            branch: "b".into(),
            into: Some("release".into()),
            rm: false,
            promote_effects: false,
            json: false
        }
    );
    assert_eq!(
        parse_str("rm b --keep-credentials").unwrap(),
        Command::Rm {
            branch: "b".into(),
            keep_credentials: true,
            json: false
        }
    );
    assert!(err("merge b --into").contains("a value is required for '--into <TARGET>'"));
    assert_eq!(
        parse_str("merge b --rm").unwrap(),
        Command::Merge {
            branch: "b".into(),
            into: None,
            rm: true,
            promote_effects: false,
            json: false
        }
    );
}

#[test]
fn workspace_actions_parse_with_json_anywhere() {
    let workspace = |line: &str| match parse_str(line).unwrap() {
        Command::Workspace { json, action } => (json, action),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        workspace("workspace show"),
        (false, WorkspaceAction::Show { branch: None })
    );
    assert_eq!(
        workspace("workspace --json show b"),
        (
            true,
            WorkspaceAction::Show {
                branch: Some("b".into())
            }
        )
    );
    assert_eq!(
        workspace("workspace trust"),
        (false, WorkspaceAction::Trust)
    );
    assert_eq!(
        workspace("workspace untrust --json"),
        (true, WorkspaceAction::Untrust)
    );
    assert_eq!(
        workspace("workspace run b dev --detach"),
        (
            false,
            WorkspaceAction::Run {
                args: vec!["b".into(), "dev".into()],
                detach: true
            }
        )
    );
    assert_eq!(
        workspace("workspace run"),
        (
            false,
            WorkspaceAction::Run {
                args: vec![],
                detach: false
            }
        )
    );
    assert_eq!(
        parse_str("workspace run a b c --detach").unwrap(),
        Command::Workspace {
            json: false,
            action: WorkspaceAction::Run {
                args: vec!["a".into(), "b".into(), "c".into()],
                detach: true,
            },
        }
    );
    assert_eq!(
        parse_str("workspace kill b --port 5173 --yes --json").unwrap(),
        Command::Workspace {
            json: true,
            action: WorkspaceAction::Kill {
                branch: Some("b".into()),
                port: Some(5173),
                yes: true,
            },
        }
    );
    assert!(err("workspace browse b --port 0x1").contains("invalid value"));
    assert!(err("workspace").contains("Usage"));
}

#[test]
fn env_actions_parse() {
    let env = |line: &str| match parse_str(line).unwrap() {
        Command::Env { json, action } => (json, action),
        other => panic!("{other:?}"),
    };
    assert_eq!(env("env list --json"), (true, EnvAction::List));
    assert_eq!(env("env show"), (false, EnvAction::Show { key: None }));
    assert_eq!(env("env rebuild"), (false, EnvAction::Rebuild));
    assert_eq!(
        env("env prune abc def --keep 1 --older-than 7"),
        (
            false,
            EnvAction::Prune {
                keys: vec!["abc".into(), "def".into()],
                keep: Some(1),
                older_than: Some(7),
            }
        )
    );
    assert_eq!(
        env("env pool fill --json"),
        (true, EnvAction::Pool(PoolAction::Fill))
    );
    assert_eq!(
        env("env pool status"),
        (false, EnvAction::Pool(PoolAction::Status))
    );
    assert_eq!(
        env("env pool drain"),
        (false, EnvAction::Pool(PoolAction::Drain))
    );
    assert!(err("env pool").contains("Usage"));
    assert!(err("env").contains("Usage"));
}

#[test]
fn help_and_version() {
    let cli = |line: &str| parse_from(format!("by {line}").split_whitespace());
    assert_eq!(parse_from(["by"]).unwrap().command, None);
    for (line, kind) in [
        ("help", ErrorKind::DisplayHelp),
        ("--help", ErrorKind::DisplayHelp),
        ("-h", ErrorKind::DisplayHelp),
        ("help merge", ErrorKind::DisplayHelp),
        ("help graph apply", ErrorKind::DisplayHelp),
        ("fan -h", ErrorKind::DisplayHelp),
        ("--version", ErrorKind::DisplayVersion),
        ("-V", ErrorKind::DisplayVersion),
    ] {
        let error = cli(line).unwrap_err();
        assert_eq!(error.kind(), kind, "{line}");
        assert_eq!(error.exit_code(), 0, "{line}");
    }
    // --help wins over missing arguments, and over anything after it.
    assert_eq!(
        cli("run --help --bogus").unwrap_err().kind(),
        ErrorKind::DisplayHelp
    );
    assert!(cli("help merge")
        .unwrap_err()
        .to_string()
        .contains("Usage: by merge [OPTIONS] <BRANCH>"));
    assert!(cli("-V").unwrap_err().to_string().starts_with("by "));
    assert!(err("help nope").contains("unrecognized subcommand 'nope'"));
}

#[test]
fn typos_get_suggestions_and_usage_errors_exit_2() {
    let error = parse_str("mrege b").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidSubcommand);
    assert_eq!(error.exit_code(), 2);
    // `review`, `recipe` and `remote` are close to `mrege` too, so clap lists them.
    assert!(
        error
            .to_string()
            .contains("similar subcommands exist: 'review', 'recipe', 'remote', 'merge'"),
        "{error}"
    );
    assert!(err("run go --budget 2").contains("a similar argument exists: '--budget-usd'"));
    assert!(err("graph shwo").contains("a similar subcommand exists: 'show'"));
}

#[test]
fn flag_errors_name_the_command() {
    let error = parse_str("run go --bogus").unwrap_err().to_string();
    assert!(error.contains("unexpected argument '--bogus'"), "{error}");
    assert!(error.contains("Usage: by run"), "{error}");
    assert!(err("nope").contains("unrecognized subcommand 'nope'"));
    assert!(err("run go -x").contains("unexpected argument '-x'"));
    assert!(err("run go --yes --ask").contains("'--yes' cannot be used with '--ask'"));
    assert!(err("run go --yes=1").contains("unexpected value '1' for '--yes'"));
    assert!(err("run go --name a --name b").contains("cannot be used multiple times"));
    assert!(err("run go --harness").contains("a value is required for '--harness <ID>'"));
    assert!(err("run go --budget-usd -1").contains("positive number"));
    assert!(err("run go --budget-usd NaN").contains("positive number"));
    assert!(err("run go --max-turns 0").contains("positive whole number"));
    assert!(err("run go --max-turns 1.5").contains("positive whole number"));
    assert!(err("run go --max-minutes 0").contains("positive number of minutes"));
    assert!(err("run go --max-minutes 1e300").contains("too large"));
    assert!(err("run go --command ''").contains("needs an executable"));
    assert!(err("send b go --isolated").contains("unexpected argument '--isolated'"));
    assert!(err("run go --check ''").contains("needs a command"));
    assert!(err("run go --check '\"cargo'").contains("unterminated quote"));
    let spaces = err("run fix the test");
    assert!(spaces.contains("unexpected argument 'the'"), "{spaces}");
    assert!(
        spaces.contains("tip: quote a prompt that contains spaces"),
        "{spaces}"
    );
    let ls = err("ls extra");
    assert!(ls.contains("unexpected argument 'extra'") && !ls.contains("quote"));
    // A checked flag's error names its own command's usage.
    assert!(err("fork b go --image x").contains("Usage: by fork"));
}

#[test]
fn globals_go_before_or_after_the_command() {
    let before = parse_from(
        split_words("by --remote http://h:1 --token-file=t --repo app ls --json").unwrap(),
    )
    .unwrap();
    let expected = Globals {
        remote: Some("http://h:1".into()),
        token_file: Some("t".into()),
        repo: Some("app".into()),
        ..Globals::default()
    };
    assert_eq!(before.globals, expected);
    let quiet = parse_from(split_words("by watch --no-notify").unwrap()).unwrap();
    assert!(quiet.globals.no_notify);
    assert_eq!(before.command, Some(Command::Ls { json: true }));
    let after = parse_from(
        split_words("by ls --json --remote http://h:1 --repo app --token-file t").unwrap(),
    )
    .unwrap();
    assert_eq!(after, before);
    let nested = parse_from(split_words("by artifact list --ca-file ca.pem").unwrap()).unwrap();
    assert_eq!(nested.globals.ca_file.as_deref(), Some("ca.pem"));
    for (line, error) in [
        ("by --remote", "a value is required for '--remote <URL>'"),
        ("by --remote= ls", "needs a value"),
        ("by --repo a --repo b ls", "cannot be used multiple times"),
        ("by ls --repo a --repo b", "cannot be used multiple times"),
    ] {
        let got = parse_from(split_words(line).unwrap())
            .unwrap_err()
            .to_string();
        assert!(got.contains(error), "{line}: {got}");
    }
}

#[test]
fn watch_and_serve() {
    assert_eq!(
        parse_str("watch").unwrap(),
        Command::Watch {
            interval: Duration::from_secs(1),
            once: false
        }
    );
    assert_eq!(
        parse_str("watch --interval 0.5 --once").unwrap(),
        Command::Watch {
            interval: Duration::from_millis(500),
            once: true
        }
    );
    assert_eq!(
        parse_str("watch --interval 250ms").unwrap(),
        Command::Watch {
            interval: Duration::from_millis(250),
            once: false
        }
    );
    assert!(err("watch --interval 0").contains("from 0.05 to 3600"));
    assert!(err("watch --interval 2h").contains("from 0.05 to 3600"));
    let call = |line: &str| {
        let args: Vec<OsString> = split_words(line)
            .unwrap()
            .into_iter()
            .map(Into::into)
            .collect();
        server_call(&args)
    };
    assert_eq!(
        call("serve --listen 127.0.0.1:0 --repo a=/r --help"),
        Some(ServerCall {
            prefix: 1,
            program: "by serve",
            args: vec![
                "--listen".into(),
                "127.0.0.1:0".into(),
                "--repo".into(),
                "a=/r".into(),
                "--help".into()
            ],
            help: false,
        })
    );
    assert_eq!(
        call("--remote=x --repo app worker --database postgres://h/d"),
        Some(ServerCall {
            prefix: 4,
            program: "by worker",
            args: vec![
                "--worker".into(),
                "--database".into(),
                "postgres://h/d".into()
            ],
            help: false,
        })
    );
    assert_eq!(
        call("--repo a help worker").map(|c| (c.prefix, c.help, c.program)),
        Some((2, true, "by worker"))
    );
    assert_eq!(call("ls serve"), None);
    assert_eq!(call("run serve"), None);
    assert_eq!(call("help merge"), None);
    assert_eq!(call(""), None);
}

#[test]
fn nested_commands_map_to_their_actions() {
    let graph = |line: &str| match parse_str(line).unwrap() {
        Command::Graph { json, action } => action.into_args(json),
        other => panic!("{other:?}"),
    };
    let show = graph("graph show root --json");
    assert_eq!(
        (show.action.as_str(), show.arg.as_deref(), show.json),
        ("show", Some("root"), true)
    );
    // The Python module puts --json before the action.
    assert!(graph("graph --json show").json);
    let apply = graph("graph apply --edits [] --expected-revision 3 --parent p --yes");
    assert_eq!(apply.action, "apply");
    assert_eq!(apply.edits.as_deref(), Some("[]"));
    assert_eq!(apply.expected_revision, Some(3));
    assert_eq!(apply.parent.as_deref(), Some("p"));
    assert_eq!(apply.task.permissions, Permissions::Yes);
    assert_eq!(graph("graph apply - --parent p").arg.as_deref(), Some("-"));
    assert_eq!(
        graph("graph resume --ask").task.permissions,
        Permissions::Ask
    );
    assert!(err("graph apply --parent p").contains("<FILE|--edits <JSON>>"));
    assert!(err("graph apply f --edits []").contains("cannot be used with"));
    assert!(err("graph apply --edits []").contains("--expected-revision"));
    assert!(err("graph").contains("Usage: by graph"));

    let rig = |line: &str| match parse_str(line).unwrap() {
        Command::Rig { json, action } => action.into_args(json),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        rig("rig check team.toml --json"),
        RigArgs {
            file: "team.toml".into(),
            json: true,
            ..RigArgs::default()
        }
    );
    let run = rig("rig run team.toml 'build it' -n t --command '/bin/agent -x'");
    assert_eq!(run.prompt.as_deref(), Some("build it"));
    assert_eq!(run.name.as_deref(), Some("t"));
    assert_eq!(run.command, Some(vec!["/bin/agent".into(), "-x".into()]));
    assert!(err("rig run team.toml").contains("<PROMPT>"));
    assert!(err("rig run team.toml ' '").contains("needs a prompt"));
    assert!(err("rig check team.toml extra").contains("unexpected argument 'extra'"));
    assert!(err("rig check team.toml --name x").contains("unexpected argument '--name'"));
    assert!(err("rig start team.toml").contains("unrecognized subcommand 'start'"));

    let artifact = |line: &str| match parse_str(line).unwrap() {
        Command::Artifact {
            branch,
            json,
            action,
        } => action.into_args(branch, json),
        other => panic!("{other:?}"),
    };
    let publish =
        artifact("artifact publish out.txt -n report --label k=v --label a=b=c --branch kid");
    assert_eq!(publish.action, "publish");
    assert_eq!(publish.arg.as_deref(), Some("out.txt"));
    assert_eq!(publish.name.as_deref(), Some("report"));
    assert_eq!(
        publish.labels,
        [("k".into(), "v".into()), ("a".into(), "b=c".into())]
    );
    assert_eq!(publish.branch.as_deref(), Some("kid"));
    assert!(artifact("artifact --json list").json);
    let export = artifact("artifact export 1 2 -o b.tar");
    assert_eq!(export.ids, ["1", "2"]);
    assert_eq!(export.out.as_deref(), Some("b.tar"));
    assert!(err("artifact get 1").contains("--out <PATH>"));
    assert!(err("artifact share 1").contains("--to <BRANCH>"));
    assert!(err("artifact export -o b.tar").contains("<IDS>..."));
    assert!(err("artifact publish").contains("<FILE>"));
    assert!(err("artifact publish f --label novalue").contains("KEY=VALUE"));
    assert!(err("artifact list --out x").contains("unexpected argument '--out'"));

    let scratch = |line: &str| match parse_str(line).unwrap() {
        Command::Scratch {
            branch,
            json,
            action,
        } => action.into_args(branch, json),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        scratch("scratch share cache --to buddy --branch me --json"),
        ScratchArgs {
            action: "share".into(),
            name: Some("cache".into()),
            to: Some("buddy".into()),
            branch: Some("me".into()),
            json: true,
        }
    );
    assert_eq!(scratch("scratch list").name, None);
    assert!(err("scratch lock").contains("<NAME>"));
    assert!(err("scratch share cache").contains("--to <BRANCH>"));
}

#[test]
fn double_dash_allows_prompts_that_look_like_flags() {
    let Command::Run { prompt, task, .. } = parse_str("run --yes -- --explain-flags").unwrap()
    else {
        panic!("not run")
    };
    assert_eq!(prompt, "--explain-flags");
    assert_eq!(task.permissions, Permissions::Yes);
}

#[test]
fn split_words_follows_shell_quoting() {
    let split = |s: &str| split_words(s).unwrap();
    assert_eq!(split("  cargo   test  "), ["cargo", "test"]);
    assert_eq!(split("sh -c 'echo $HOME'"), ["sh", "-c", "echo $HOME"]);
    assert_eq!(
        split(r#"say "a \"quoted\" \$word" and\ more"#),
        ["say", r#"a "quoted" $word"#, "and more"]
    );
    assert_eq!(split(r#""keep \n literal""#), [r"keep \n literal"]);
    assert_eq!(split("''"), [""]);
    assert_eq!(split("a'b'\"c\""), ["abc"]);
    assert_eq!(split("make test # a comment"), ["make", "test"]);
    assert_eq!(split("grep a#b"), ["grep", "a#b"]);
    assert!(split("").is_empty());
    assert!(split_words("a\\").is_err());
    assert!(split_words("'a").is_err());
}

#[test]
fn shell_quote_round_trips() {
    assert_eq!(shell_quote("fix-flaky_1.2"), "fix-flaky_1.2");
    assert_eq!(shell_quote("it's here"), "\"it's here\"");
    assert_eq!(shell_quote(""), "''");
    for word in [
        "plain",
        "two words",
        "it's",
        "$x",
        "",
        "a\"b'c",
        "tab\there",
    ] {
        assert_eq!(split_words(&shell_quote(word)).unwrap(), [word], "{word}");
    }
}

fn help(name: &str) -> String {
    let mut cmd = command();
    cmd.build();
    cmd.find_subcommand_mut(name)
        .unwrap()
        .render_help()
        .to_string()
}

/// The commands a harness runs for itself, and plumbing: not in `by
/// --help`, but in `by help all`, and the first group in `by help
/// agents`.
const HIDDEN: &[&str] = &[
    "ask",
    "answer",
    "inbox",
    "escalate",
    "report",
    "report-status",
    "brief",
    "delegate",
    "spawn",
    "children",
    "graph",
    "integrate",
    "inspect",
    "events",
    "discard",
    "mcp",
    "hook",
    "statusline",
    "completions",
    "man",
    "worker",
    "serve",
];

/// Every command and subcommand under `cmd`, with the path that names it.
fn walk<'a>(cmd: &'a clap::Command, path: &str, out: &mut Vec<(String, &'a clap::Command)>) {
    for sub in cmd.get_subcommands().filter(|sub| sub.get_name() != "help") {
        let name = format!("{path} {}", sub.get_name());
        out.push((name.clone(), sub));
        walk(sub, &name, out);
    }
}

fn built() -> clap::Command {
    let mut cmd = command();
    cmd.build();
    cmd
}

#[test]
fn help_lists_the_first_run_commands_and_help_all_every_command() {
    let general = command().render_help().to_string();
    for (title, names) in FRONT {
        for name in *names {
            if built().find_subcommand(name).is_some() {
                assert!(
                    general.contains(&format!("\n  {name} ")),
                    "{name} missing from:\n{general}"
                );
            }
        }
        assert!(general.contains(&format!("{title}:")), "{title}");
    }
    // What an agent or the plumbing runs is not on the first screen.
    for name in HIDDEN {
        assert!(
            !general.contains(&format!("\n  {name} ")),
            "{name} is listed in:\n{general}"
        );
    }
    // check, wait and cancel are for people too.
    let visible = |name: &str| !built().find_subcommand(name).unwrap().is_hide_set();
    assert!(["check", "wait", "cancel"].into_iter().all(visible));
    assert!(general.contains("Also: "), "{general}");
    assert!(general.contains("by help all") && general.contains("by help agents"));
    assert!(
        general.lines().count() < 70,
        "{} lines:\n{general}",
        general.lines().count()
    );
    assert!(general.contains("[env: BRANCHYARD_REMOTE]"), "{general}");
    assert!(!general.contains("Other commands"));

    // `by help all` lists every command, hidden or not, in its group.
    let all = listing_text(Listing::All, false);
    let cmd = built();
    for sub in cmd.get_subcommands() {
        let name = sub.get_name();
        assert!(
            all.contains(&format!("\n  {name} ")),
            "{name} missing from:\n{all}"
        );
        if name != "help" {
            let order = sub.get_display_order();
            assert!(
                GROUPS.iter().any(|(group, _)| *group == order / 100),
                "{name} is in no group ({order})"
            );
        }
    }
    for (_, title) in GROUPS {
        assert!(all.contains(&format!("{title}:")), "{title}");
    }
    assert!(!all.contains("Other commands"));

    // `by help agents` is what a harness runs.
    let agents = listing_text(Listing::Agents, false);
    for name in ["spawn", "ask", "inbox", "wait", "check", "mcp", "hook"] {
        assert!(
            agents.contains(&format!("\n  {name} ")),
            "{name}:\n{agents}"
        );
    }
    assert!(!agents.contains("\n  run "), "{agents}");

    let run = help("run");
    for heading in [
        "Checks and limits:",
        "Permissions:",
        "Launch:",
        "Microsandbox provider",
        "Substrate provider",
        "Delegation:",
        "Provisioning:",
        "Global options:",
        "Examples:",
    ] {
        assert!(run.contains(heading), "{heading} missing from:\n{run}");
    }
    assert!(help("fork").contains("Usage: by fork [OPTIONS] <BRANCH> <PROMPT>"));
    assert!(help("ls").contains("Usage: by ls [OPTIONS]"));
    assert!(help("rm").contains("Usage: by rm [OPTIONS] <BRANCH>"));
    // A hidden command still has its help.
    assert!(help("spawn").contains("Usage: by spawn"));
}

#[test]
fn help_topics_are_recognised_after_the_global_options() {
    let argv =
        |line: &str| -> Vec<OsString> { line.split_whitespace().map(OsString::from).collect() };
    assert_eq!(help_listing(&argv("help all")), Some(Listing::All));
    assert_eq!(help_listing(&argv("help agents")), Some(Listing::Agents));
    assert_eq!(
        help_listing(&argv("--remote http://h:1 --no-notify help all")),
        Some(Listing::All)
    );
    // A command is a command: `by help run` is clap's.
    assert_eq!(help_listing(&argv("help run")), None);
    assert_eq!(help_listing(&argv("help")), None);
    assert_eq!(help_listing(&argv("run all")), None);
    assert_eq!(help_listing(&argv("help all extra")), None);
}

#[test]
fn the_agent_verbs_and_the_plumbing_are_hidden() {
    let cmd = built();
    for name in HIDDEN {
        let sub = cmd
            .find_subcommand(name)
            .unwrap_or_else(|| panic!("no {name}"));
        assert!(sub.is_hide_set(), "{name} is listed");
    }
    // Everything else is listed, so `by help all` and the shell see it.
    let shown: Vec<&str> = cmd
        .get_subcommands()
        .filter(|c| !c.is_hide_set())
        .map(|c| c.get_name())
        .collect();
    for name in [
        "run", "fan", "send", "check", "wait", "cancel", "merge", "doctor",
    ] {
        assert!(shown.contains(&name), "{name} is hidden");
    }
    // The man page lists them all.
    let mut page = Vec::new();
    clap_mangen::Man::new(command_with_hidden())
        .render(&mut page)
        .unwrap();
    let page = String::from_utf8(page).unwrap();
    assert!(
        page.contains("by\\-spawn") && page.contains("by\\-hook"),
        "{page}"
    );
}

#[test]
fn a_summary_is_one_short_line() {
    let cmd = built();
    let mut all = Vec::new();
    walk(&cmd, "by", &mut all);
    let mut long = Vec::new();
    for (path, sub) in &all {
        let about = sub.get_about().map(|a| a.to_string()).unwrap_or_default();
        let top = path.matches(' ').count() == 1;
        let limit = if top { 80 } else { 100 };
        if about.chars().count() >= limit || about.contains('\n') {
            long.push(format!("{path} ({}): {about}", about.chars().count()));
        }
    }
    assert!(
        long.is_empty(),
        "summaries over the limit:\n{}",
        long.join("\n")
    );
}

#[test]
fn every_argument_says_what_it_is() {
    let cmd = built();
    let mut all = Vec::new();
    walk(&cmd, "by", &mut all);
    let mut blank = Vec::new();
    for (path, sub) in &all {
        for arg in sub.get_arguments() {
            let id = arg.get_id().as_str();
            if arg.is_hide_set() || matches!(id, "help" | "version") {
                continue;
            }
            if arg
                .get_help()
                .is_none_or(|h| h.to_string().trim().is_empty())
            {
                blank.push(format!("{path} {id}"));
            }
        }
    }
    assert!(
        blank.is_empty(),
        "arguments with no help:\n{}",
        blank.join("\n")
    );
}

#[test]
fn placeholders_say_what_they_take() {
    let cmd = built();
    let mut all = Vec::new();
    walk(&cmd, "by", &mut all);
    let mut vague = Vec::new();
    for (path, sub) in &all {
        for arg in sub.get_arguments() {
            for name in arg.get_value_names().unwrap_or_default() {
                if ["X", "S", "SECS"].contains(&name.as_str()) {
                    vague.push(format!("{path} {} <{name}>", arg.get_id()));
                }
            }
        }
    }
    assert!(
        vague.is_empty(),
        "vague placeholders:\n{}",
        vague.join("\n")
    );
    let run = help("run");
    assert!(run.contains("--budget-usd <DOLLARS>"), "{run}");
    assert!(help("wait").contains("--timeout <SECONDS>"));
}

#[test]
fn yes_says_what_it_does_in_each_command() {
    let text = |command: &str| help(command);
    assert!(
        text("run").contains("Grant every tool permission request without asking")
            && text("run").contains("the harness may write files and run commands"),
        "{}",
        text("run")
    );
    assert!(text("rewind").contains("Confirm the reset without a prompt"));
    assert!(text("undo").contains("Undo every reversible effect without a confirmation"));
    assert!(text("compare").contains("Confirm removing the other attempts without a prompt"));
    assert!(text("up").contains("Make the installs the [harnesses] policy would ask about"));
    // No command leaves it at the vague wording.
    let cmd = built();
    let mut all = Vec::new();
    walk(&cmd, "by", &mut all);
    for (path, sub) in &all {
        for arg in sub.get_arguments().filter(|a| a.get_id() == "yes") {
            let help = arg.get_help().map(|h| h.to_string()).unwrap_or_default();
            for vague in [
                "Do not ask first",
                "Do not ask for confirmation",
                "Allow every tool",
            ] {
                assert!(!help.contains(vague), "{path} --yes: {help}");
            }
        }
    }
}

#[test]
fn run_and_fork_take_json_without_detach() {
    let Command::Run { detached, .. } = parse_str("run go --json").unwrap() else {
        panic!("not run")
    };
    assert!(detached.json && !detached.detach);
    let Command::Fork { detached, .. } = parse_str("fork b go --json").unwrap() else {
        panic!("not fork")
    };
    assert!(detached.json && !detached.detach);
    let Command::Run { detached, .. } = parse_str("run go --detach --json").unwrap() else {
        panic!("not run")
    };
    assert!(detached.json && detached.detach);
    // The commands that gained --json this round.
    for line in [
        "merge b --json",
        "rm b --json",
        "fan go --harness codex --json",
        "review b --json",
    ] {
        assert!(parse_str(line).is_ok(), "{line}: {}", err_or_ok(line));
    }
    assert!(parse_str("review b --detach --json").is_err());
}

fn err_or_ok(line: &str) -> String {
    parse_str(line)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default()
}

#[test]
fn the_run_help_defaults_say_the_command_waits() {
    for command in ["run", "send", "fork", "fan"] {
        let cmd = built();
        let about = cmd
            .find_subcommand(command)
            .unwrap()
            .get_about()
            .unwrap()
            .to_string();
        assert!(about.contains("waits"), "{command}: {about}");
    }
    let detach = help("run");
    assert!(
        detach.contains(
            "Return as soon as the turn has started, leaving it running in the background"
        ),
        "{detach}"
    );
    assert!(help("send").contains("Return as soon as the turn has started"));
}

#[test]
fn completions_and_the_man_page_generate() {
    for shell in [
        clap_complete::Shell::Bash,
        clap_complete::Shell::Zsh,
        clap_complete::Shell::Fish,
        clap_complete::Shell::PowerShell,
        clap_complete::Shell::Elvish,
    ] {
        let mut out = Vec::new();
        clap_complete::generate(shell, &mut command(), "by", &mut out);
        let script = String::from_utf8(out).unwrap();
        for word in [
            "reincarnate",
            "budget-usd",
            "remote",
            "doctor",
            "init",
            "dry-run",
            "config",
            "validate",
        ] {
            assert!(script.contains(word), "{shell}: {word}");
        }
    }
    let mut page = Vec::new();
    clap_mangen::Man::new(command()).render(&mut page).unwrap();
    let page = String::from_utf8(page).unwrap();
    assert!(page.contains(".TH by") && page.contains("remote"), "{page}");
    assert!(
        page.contains("by\\-init") && page.contains("by\\-config"),
        "{page}"
    );
}

fn init(line: &str) -> InitArgs {
    match parse_str(line).unwrap() {
        Command::Init { init } => init.into_inner(),
        other => panic!("{line}: parsed as {other:?}"),
    }
}

#[test]
fn init_takes_a_topic_and_one_protocol_step() {
    use branchyard_setup::Topic;
    assert_eq!(
        init("init"),
        InitArgs {
            topic: None,
            step: None,
            json: false,
            answers: None,
            defaults: false,
            force: false,
            harnesses: Vec::new(),
            all_detected: false,
            status: false,
        }
    );
    assert!(init("init plugin --status").status);
    assert!(init("init plugin --status --json").json);
    let plugin = init("init plugin codex hermes --dry-run");
    assert_eq!(plugin.harnesses, ["codex", "hermes"]);
    assert_eq!(plugin.step, Some(InitStep::DryRun));
    assert!(init("init plugin --all-detected").all_detected);
    assert!(init("init plugin --json").json);
    assert_eq!(init("init --json").step, None);
    assert_eq!(init("init server --defaults").topic, Some(Topic::Server));
    let next = init("init project --json --next --answers=a.json");
    assert_eq!(next.topic, Some(Topic::Project));
    assert_eq!(next.step, Some(InitStep::Next));
    assert!(next.json);
    assert_eq!(next.answers.as_deref(), Some("a.json"));
    let apply = init("init rig --answers - --apply --force --defaults");
    assert_eq!(apply.step, Some(InitStep::Apply));
    assert!(apply.force && apply.defaults);
    assert_eq!(apply.answers.as_deref(), Some("-"));
    assert_eq!(init("init deploy --dry-run").step, Some(InitStep::DryRun));
    // Globals still go anywhere.
    assert_eq!(
        init("init plugin --remote http://h:1 --next").topic,
        Some(Topic::Plugin)
    );
    // Every topic the engine knows parses, in its order.
    for topic in Topic::ALL {
        assert_eq!(init(&format!("init {}", topic.id())).topic, Some(topic));
    }
}

fn setup(line: &str) -> SetupFlags {
    match parse_str(line).unwrap() {
        Command::Setup { setup } => setup,
        other => panic!("{line}: parsed as {other:?}"),
    }
}

#[test]
fn setup_takes_its_flags_and_is_the_first_command_listed() {
    assert_eq!(setup("setup"), SetupFlags::default());
    let all = setup("setup --yes --dry-run --json --no-config");
    assert!(all.yes && all.dry_run && all.json && all.no_config && !all.no_hooks);
    // Harnesses, comma-separated or as several values.
    assert_eq!(
        setup("setup --harness claude-code,codex").harnesses,
        ["claude-code", "codex"]
    );
    assert_eq!(
        setup("setup --yes --harness claude-code codex --no-config").harnesses,
        ["claude-code", "codex"]
    );
    assert!(setup("setup --no-hooks").no_hooks);
    // Ask the harnesses again rather than read the cached inventory.
    assert!(setup("setup --dry-run --refresh").refresh);
    // Naming harnesses and installing none contradict.
    let error = parse_str("setup --harness codex --no-hooks").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ArgumentConflict, "{error}");
    assert_eq!(error.exit_code(), 2);
    assert_eq!(
        parse_str("setup --bogus").unwrap_err().kind(),
        ErrorKind::UnknownArgument
    );
    // "Start" leads the first screen, with setup first in it; `by help
    // all` files it under "Shell and setup".
    let general = command().render_help().to_string();
    let start = general.find("Start:").unwrap();
    assert!(start < general.find("Work:").unwrap(), "{general}");
    assert!(
        general[start..].starts_with("Start:\n  setup "),
        "{general}"
    );
    let all = listing_text(Listing::All, false);
    let shell = all.find("Shell and setup:").unwrap();
    assert!(all[shell..].contains("\n  setup "), "{all}");
    let help = help("setup");
    for word in [
        "--yes",
        "--dry-run",
        "--json",
        "--harness <ID>",
        "--no-hooks",
        "--no-config",
    ] {
        assert!(help.contains(word), "{word} missing from:\n{help}");
    }
    assert!(help.contains("docs/setup.md#one-command"), "{help}");
}

#[test]
fn init_refuses_conflicting_or_incomplete_steps_as_usage_errors() {
    for (line, kind, text) in [
        (
            "init project --next --apply",
            ErrorKind::ArgumentConflict,
            "separate steps",
        ),
        (
            "init project --dry-run --apply",
            ErrorKind::ArgumentConflict,
            "separate steps",
        ),
        (
            "init project --next --dry-run",
            ErrorKind::ArgumentConflict,
            "separate steps",
        ),
        (
            "init --next",
            ErrorKind::MissingRequiredArgument,
            "give a topic",
        ),
        (
            "init project --force",
            ErrorKind::MissingRequiredArgument,
            "--apply",
        ),
        (
            "init project --force --dry-run",
            ErrorKind::ArgumentConflict,
            "'--force' cannot be used with '--dry-run'",
        ),
        (
            "init project --answers a.json",
            ErrorKind::MissingRequiredArgument,
            "--next",
        ),
        (
            "init nope --next",
            ErrorKind::ValueValidation,
            "unknown topic 'nope'",
        ),
        (
            "init project extra",
            ErrorKind::ArgumentConflict,
            "harnesses, --all-detected and --status go with by init plugin",
        ),
        (
            "init rig --all-detected",
            ErrorKind::ArgumentConflict,
            "go with by init plugin",
        ),
        (
            "init rig --status",
            ErrorKind::ArgumentConflict,
            "go with by init plugin",
        ),
        (
            "init plugin --status --apply",
            ErrorKind::ArgumentConflict,
            "cannot be used with",
        ),
        (
            "init plugin codex --all-detected",
            ErrorKind::ArgumentConflict,
            "cannot be used with",
        ),
        (
            "init project --bogus",
            ErrorKind::UnknownArgument,
            "--bogus",
        ),
        (
            "init project --json --json",
            ErrorKind::ArgumentConflict,
            "cannot be used multiple times",
        ),
        (
            "init project --answers",
            ErrorKind::InvalidValue,
            "a value is required",
        ),
        (
            "init project --json",
            ErrorKind::ArgumentConflict,
            "give --next, --dry-run or --apply",
        ),
        (
            "init --json --apply=yes",
            ErrorKind::TooManyValues,
            "--apply",
        ),
    ] {
        let error = parse_str(line).unwrap_err();
        assert_eq!(error.kind(), kind, "{line}: {error}");
        assert_eq!(error.exit_code(), 2, "{line}");
        let text_of = error.to_string();
        assert!(text_of.contains(text), "{line}: {text_of}");
        // clap shows the usage line with every error but a bad value.
        if !matches!(kind, ErrorKind::ValueValidation | ErrorKind::InvalidValue) {
            assert!(text_of.contains("Usage: by init"), "{line}: {text_of}");
        }
    }
    let help = help("init");
    for word in [
        "[TOPIC]",
        "--next",
        "--dry-run",
        "--apply",
        "--force",
        "--answers <FILE|->",
        "Examples:",
    ] {
        assert!(help.contains(word), "{word} missing from:\n{help}");
    }
    assert!(
        help.contains("[possible values: project, server, rig, deploy, plugin]"),
        "{help}"
    );
}

#[test]
fn config_has_four_actions() {
    let config = |line: &str| match parse_str(line).unwrap() {
        Command::Config { json, action } => (json, action),
        other => panic!("{line}: parsed as {other:?}"),
    };
    assert_eq!(config("config show"), (false, ConfigAction::Show));
    assert_eq!(config("config --json show"), (true, ConfigAction::Show));
    assert_eq!(config("config path --json"), (true, ConfigAction::Path));
    assert_eq!(
        config("config validate"),
        (false, ConfigAction::Validate { file: None })
    );
    assert_eq!(
        config("config validate b.toml --json"),
        (
            true,
            ConfigAction::Validate {
                file: Some("b.toml".into())
            }
        )
    );
    assert_eq!(config("config schema"), (false, ConfigAction::Schema));
    for (line, kind) in [
        (
            "config",
            ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand,
        ),
        ("config shwo", ErrorKind::InvalidSubcommand),
        ("config show extra", ErrorKind::UnknownArgument),
        ("config validate a b", ErrorKind::UnknownArgument),
        ("config schema --bogus", ErrorKind::UnknownArgument),
    ] {
        let error = parse_str(line).unwrap_err();
        assert_eq!(error.kind(), kind, "{line}: {error}");
        assert_eq!(error.exit_code(), 2, "{line}");
    }
    assert!(err("config shwo").contains("a similar subcommand exists: 'show'"));
    assert!(help("config").contains("BRANCHYARD_USER_CONFIG"));
}

#[test]
fn up_takes_plan_yes_and_json() {
    let up = |line: &str| match parse_str(line).unwrap() {
        Command::Up { json, plan, yes } => (json, plan, yes),
        other => panic!("{line}: parsed as {other:?}"),
    };
    assert_eq!(up("up"), (false, false, false));
    assert_eq!(up("up --plan --json"), (true, true, false));
    assert_eq!(up("up --yes"), (false, false, true));
    assert!(parse_str("up extra").is_err());
}

#[test]
fn doctor_takes_json_and_a_path_and_the_global_server_options() {
    let doctor = |line: &str| match parse_str(line).unwrap() {
        Command::Doctor { json, path, .. } => (json, path),
        other => panic!("{line}: parsed as {other:?}"),
    };
    assert!(matches!(
        parse_str("doctor --refresh").unwrap(),
        Command::Doctor { refresh: true, .. }
    ));
    assert_eq!(doctor("doctor"), (false, None));
    assert_eq!(doctor("doctor --json"), (true, None));
    assert_eq!(
        doctor("doctor --path ~/src/app --json"),
        (true, Some("~/src/app".into()))
    );
    // `--repo` is the global option naming a repository on a server, not
    // a path; doctor leaves it to the server check.
    let cli = parse_from(split_words("by doctor --repo app --remote http://h:1").unwrap());
    let cli = cli.unwrap();
    assert_eq!(cli.globals.repo.as_deref(), Some("app"));
    assert_eq!(cli.globals.remote.as_deref(), Some("http://h:1"));
    for line in ["doctor extra", "doctor --path", "doctor --bogus"] {
        let error = parse_str(line).unwrap_err();
        assert_eq!(error.exit_code(), 2, "{line}: {error}");
    }
    let text = help("doctor");
    for needle in [
        "--json",
        "--path <PATH>",
        "Exit status: 0",
        "docs/doctor.md",
    ] {
        assert!(text.contains(needle), "{needle} missing from:\n{text}");
    }
}

/// docs/cli-reference.md is what `by man --markdown` renders now: a flag or
/// command added, renamed or reworded here is in the reference too.
#[test]
fn the_cli_reference_is_current() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/cli-reference.md");
    let checked_in = std::fs::read_to_string(&path).unwrap();
    assert!(
        checked_in == reference_markdown(),
        "docs/cli-reference.md is stale; regenerate it with `python3 tools/check.py --fix` (or \
         cargo run -q -p branchyard-cli --bin by -- man --markdown > docs/cli-reference.md)"
    );
}

#[test]
fn the_reference_lists_every_visible_command_once() {
    let text = reference_markdown();
    for sub in command().get_subcommands().filter(|s| !s.is_hide_set()) {
        if sub.get_name() == "help" {
            continue;
        }
        let heading = format!("## `by {}`\n", sub.get_name());
        assert_eq!(text.matches(&heading).count(), 1, "{heading}");
    }
    // Hidden plumbing stays out, as it does in `by --help`.
    assert!(!text.contains("## `by hook`"));
    assert!(text.contains("| `--remote <URL>` |"), "the global options");
}
