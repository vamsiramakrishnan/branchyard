// Partly derived from Scion (https://github.com/GoogleCloudPlatform/scion)
// at d9b9e6a2e1e29e428e6f8e72c2d5ab0df0475338: cases of
// harnesses/claude/provision_test.py, harnesses/codex/provision_test.py and
// harnesses/telemetry_provision_test.py. Copyright 2026 Google LLC. Licensed
// under the Apache License, Version 2.0.
//
// Modified for Branchyard: translated to Rust against provisioning plans
// applied to temporary homes and compared with golden files.

//! Provisioning plans applied to temporary homes, compared with golden
//! files under `tests/golden/<case>/`: the home's files after provisioning
//! (by relative path) and `env.txt`, the harness variables, secrets shown
//! as `<secret>`. Set `BY_UPDATE_GOLDEN=1` to rewrite them, then review the
//! diff. Cases follow Scion's provisioner tests at the pinned revision;
//! each says which.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use branchyard_provision::apply::{apply, installed};
use branchyard_provision::{
    for_harness, provisioners, Context, Effort, Instructions, McpServer, Plan, Protocol, Secret,
    Telemetry, NOT_PORTED,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A temporary directory, removed on drop.
struct Temp(PathBuf);

impl Temp {
    fn new() -> Temp {
        let dir = std::env::temp_dir().join(format!(
            "by-provision-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("home")).unwrap();
        Temp(dir)
    }

    fn home(&self) -> PathBuf {
        self.0.join("home")
    }

    fn seed(&self, relative: &str, content: &str) {
        let path = self.home().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn read(&self, relative: &str) -> String {
        fs::read_to_string(self.home().join(relative)).unwrap()
    }

    fn mode(&self, relative: &str) -> u32 {
        fs::metadata(self.home().join(relative))
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn context(harness: &str, protocol: Protocol) -> Context {
    let mut context = Context::new(harness, protocol, "/branchyard/home", "/workspace");
    context.private_home = true;
    context.sandbox = true;
    context
}

fn secrets(pairs: &[(&str, &str)]) -> Vec<Secret> {
    pairs.iter().map(|(n, v)| Secret::new(*n, *v)).collect()
}

fn telemetry(endpoint: &str) -> Option<Telemetry> {
    Some(Telemetry {
        enabled: true,
        endpoint: Some(endpoint.into()),
    })
}

fn env_text(plan: &Plan) -> String {
    let mut lines: Vec<String> = plan
        .env
        .iter()
        .map(|e| match e.secret {
            true => format!("{}=<secret>", e.name),
            false => format!("{}={}", e.name, e.value),
        })
        .collect();
    lines.sort();
    lines.push(format!(
        "# auth: {}",
        plan.auth.as_deref().unwrap_or("none")
    ));
    lines.push(format!("# unused: {}", plan.unused_secrets.join(",")));
    lines.join("\n") + "\n"
}

/// Every regular file under `dir`, by relative path.
fn files(dir: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in fs::read_dir(&current).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let relative = path.strip_prefix(dir).unwrap().display().to_string();
                out.insert(relative, fs::read_to_string(&path).unwrap());
            }
        }
    }
    out
}

/// Compare the home and the plan's variables with `tests/golden/<case>`.
fn golden(case: &str, temp: &Temp, plan: &Plan) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(case);
    let mut actual = files(&temp.home());
    actual.insert("env.txt".into(), env_text(plan));
    if std::env::var_os("BY_UPDATE_GOLDEN").is_some() {
        let _ = fs::remove_dir_all(&dir);
        for (relative, content) in &actual {
            let path = dir.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
    }
    let expected = files(&dir);
    assert_eq!(
        actual.keys().collect::<Vec<_>>(),
        expected.keys().collect::<Vec<_>>(),
        "files of {case}"
    );
    for (relative, content) in &expected {
        assert_eq!(&actual[relative], content, "{case}/{relative}");
    }
}

/// Plan, apply, and check a second application changes nothing.
fn provision(temp: &Temp, context: &Context) -> Plan {
    let plan = branchyard_provision::plan(context).unwrap();
    apply(&plan, &temp.home()).unwrap();
    let again = apply(&plan, &temp.home()).unwrap();
    assert!(
        again.written.is_empty() && again.removed.is_empty(),
        "re-provisioning changed {again:?}"
    );
    plan
}

const ANTHROPIC_KEY: &str = "sk-ant-api03-SECRETSECRET-0123456789abcdefghij";

#[test]
fn claude_api_key_model_and_telemetry_keep_the_users_claude_json() {
    // Scion's claude/provision.py (api-key approval, project trust) and
    // telemetry_provision_test.py (test_claude_default_custom_and_disabled).
    let temp = Temp::new();
    temp.seed(
        ".claude.json",
        r#"{"numStartups": 2, "projects": {"/elsewhere": {"allowedTools": ["Bash"]}},
            "customApiKeyResponses": {"approved": ["an-older-fingerprint"]}}"#,
    );
    temp.seed(".claude/settings.json", "{\"theme\": \"dark\"}\n");
    let mut context = context("claude-code", Protocol::ClaudeStreamJson);
    context.secrets = secrets(&[
        ("ANTHROPIC_API_KEY", ANTHROPIC_KEY),
        ("OPENAI_API_KEY", "x"),
    ]);
    context.model = Some("L".into());
    context.telemetry = telemetry("http://127.0.0.1:14317");
    let plan = provision(&temp, &context);
    golden("claude-api-key", &temp, &plan);
    assert_eq!(temp.mode(".claude.json"), 0o600);
    assert_eq!(
        temp.read(".claude/settings.json"),
        "{\"theme\": \"dark\"}\n"
    );
    let key = plan
        .env
        .iter()
        .find(|e| e.name == "ANTHROPIC_API_KEY")
        .unwrap();
    assert!(key.secret && key.value == ANTHROPIC_KEY);
}

#[test]
fn claude_telemetry_off_disables_every_exporter() {
    // telemetry_provision_test.py, the disabled case.
    let mut context = context("claude-code", Protocol::Acp);
    context.telemetry = Some(Telemetry {
        enabled: false,
        endpoint: None,
    });
    let plan = branchyard_provision::plan(&context).unwrap();
    let env: BTreeMap<_, _> = plan
        .env
        .iter()
        .map(|e| (e.name.as_str(), e.value.as_str()))
        .collect();
    assert_eq!(env["CLAUDE_CODE_ENABLE_TELEMETRY"], "0");
    assert_eq!(env["OTEL_METRICS_EXPORTER"], "none");
    assert_eq!(env["OTEL_LOGS_EXPORTER"], "none");
    assert_eq!(env["OTEL_TRACES_EXPORTER"], "none");
    assert!(plan.files.is_empty());
}

#[test]
fn claude_credentials_and_oauth_and_vertex() {
    let temp = Temp::new();
    let mut context = context("claude-code", Protocol::ClaudeStreamJson);
    context.secrets = secrets(&[("CLAUDE_AUTH", r#"{"claudeAiOauth": {"accessToken": "t"}}"#)]);
    let plan = provision(&temp, &context);
    golden("claude-auth-file", &temp, &plan);
    assert_eq!(temp.mode(".claude/.credentials.json"), 0o600);
    assert_eq!(temp.mode(".claude"), 0o700);

    context.secrets = secrets(&[
        ("CLAUDE_CODE_OAUTH_TOKEN", "oat"),
        ("GOOGLE_CLOUD_PROJECT", "p"),
        ("GOOGLE_CLOUD_REGION", "r"),
    ]);
    assert_eq!(plan_auth(&context), "oauth-token");
    context.auth = Some("vertex-ai".into());
    let vertex = branchyard_provision::plan(&context).unwrap();
    let names: Vec<_> = vertex.env.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"CLAUDE_CODE_USE_VERTEX") && names.contains(&"CLOUD_ML_REGION"));
    context.secrets = secrets(&[("CLAUDE_AUTH", "not json")]);
    context.auth = None;
    let refused = branchyard_provision::plan(&context).unwrap_err().0;
    assert!(refused.contains("not valid JSON") && !refused.contains("not json"));
}

fn plan_auth(context: &Context) -> String {
    branchyard_provision::plan(context).unwrap().auth.unwrap()
}

const USER_CODEX_CONFIG: &str = r#"# My Codex settings.
model = "gpt-5.5"
reasoning_effort = "low"

[features]
hooks = true

[projects."/workspace"]
trust_level = "trusted"

[otel]
enabled = false
metrics_exporter = "statsig"

[otel.exporter."otlp-grpc"]
endpoint = "https://external.invalid:443"
"#;

#[test]
fn codex_api_key_effort_and_telemetry_reconcile_config_toml() {
    // Scion's codex/provision.py and provision_test.py: auth.json, the
    // reasoning effort keys, and [otel] reconciliation.
    let temp = Temp::new();
    temp.seed(".codex/config.toml", USER_CODEX_CONFIG);
    let mut context = context("codex", Protocol::CodexAppServer);
    context.secrets = secrets(&[
        ("OPENAI_API_KEY", "sk-openai-secret"),
        ("ANTHROPIC_API_KEY", "x"),
    ]);
    context.effort = Some(Effort::High);
    context.model = Some("gpt-5.5-codex".into());
    context.telemetry = telemetry("http://127.0.0.1:14317");
    let plan = provision(&temp, &context);
    golden("codex-api-key", &temp, &plan);
    assert_eq!(temp.mode(".codex/auth.json"), 0o600);
    assert_eq!(temp.mode(".codex/config.toml"), 0o644);
    assert_eq!(plan.session.model.as_deref(), Some("gpt-5.5-codex"));
    let config = temp.read(".codex/config.toml");
    assert!(!config.contains("statsig") && !config.contains("external.invalid"));
}

#[test]
fn codex_acp_takes_its_model_from_config_toml_and_auth_file_is_checked() {
    let temp = Temp::new();
    let mut context = context("codex", Protocol::Acp);
    context.secrets = secrets(&[("CODEX_AUTH", r#"{"tokens": {"id_token": "x"}}"#)]);
    context.model = Some("m".into());
    context.telemetry = Some(Telemetry {
        enabled: false,
        endpoint: None,
    });
    let plan = provision(&temp, &context);
    golden("codex-acp-auth-file", &temp, &plan);
    assert!(plan.session.model.is_none());
    context.secrets = secrets(&[("CODEX_AUTH", "")]);
    assert!(branchyard_provision::plan(&context)
        .unwrap_err()
        .0
        .contains("empty"));
}

#[test]
fn gemini_settings_are_merged() {
    // Scion's gemini-cli/provision.py and telemetry_provision_test.py
    // (test_gemini_default_custom_and_disabled).
    let temp = Temp::new();
    temp.seed(
        ".gemini/settings.json",
        r#"{"general": {"disableAutoUpdate": true}, "model": {"skipNextSpeakerCheck": true},
            "security": {"folderTrust": {"enabled": false}}}"#,
    );
    let mut context = context("gemini-cli", Protocol::Acp);
    context.secrets = secrets(&[("GOOGLE_API_KEY", "g-secret")]);
    context.model = Some("gemini-3.6-flash".into());
    context.telemetry = telemetry("http://127.0.0.1:14317");
    let plan = provision(&temp, &context);
    golden("gemini-api-key", &temp, &plan);
}

#[test]
fn copilot_token_trusts_the_workspace_in_a_commented_config() {
    // Scion's copilot/provision.py (_ensure_settings, telemetry env).
    let temp = Temp::new();
    temp.seed(
        ".copilot/config.json",
        "// Written by the user.\n{\"trustedFolders\": [\"/home/me\"], \"theme\": \"dim\"}\n",
    );
    temp.seed(".copilot/settings.json", "{\"autoUpdate\": true}\n");
    let mut context = context("github-copilot", Protocol::Acp);
    context.secrets = secrets(&[("GH_TOKEN", "ghp-secret")]);
    context.telemetry = telemetry("http://127.0.0.1:14317");
    let plan = provision(&temp, &context);
    golden("copilot-token", &temp, &plan);
}

#[test]
fn hermes_env_file_keeps_other_lines() {
    let temp = Temp::new();
    temp.seed(
        ".hermes/.env",
        "# mine\nHERMES_THEME=dark\nOPENAI_API_KEY=stale\n",
    );
    let mut context = context("hermes", Protocol::Acp);
    context.secrets = secrets(&[("OPENAI_API_KEY", "sk-hermes-secret")]);
    context.model = Some("openai/gpt-5.5".into());
    let plan = provision(&temp, &context);
    golden("hermes-api-key", &temp, &plan);
    assert_eq!(temp.mode(".hermes/.env"), 0o600);
}

#[test]
fn opencode_auth_file() {
    let temp = Temp::new();
    let mut context = context("opencode", Protocol::Acp);
    context.secrets = secrets(&[("OPENCODE_AUTH", r#"{"anthropic": {"type": "api"}}"#)]);
    let plan = provision(&temp, &context);
    golden("opencode-auth-file", &temp, &plan);
    context.model = Some("x".into());
    assert!(branchyard_provision::plan(&context).is_err());
}

fn server(name: &str) -> McpServer {
    McpServer {
        name: name.into(),
        command: format!("/usr/bin/{name}-mcp"),
        args: vec!["--stdio".into()],
        env: vec![("LEVEL".into(), "1".into())],
    }
}

#[test]
fn antigravity_gets_mcp_servers_and_instructions_in_its_configuration() {
    // Scion's antigravity/provision.py: AGY_MCP_MAPPING and the
    // instructions file, merged instead of replaced.
    let temp = Temp::new();
    temp.seed(
        ".gemini/config/mcp_config.json",
        r#"{"mcpServers": {"users-own": {"command": "/opt/mine"}}}"#,
    );
    temp.seed(".gemini/GEMINI.md", "# My notes\n\nKeep these.\n");
    let mut context = context("antigravity", Protocol::AntigravityStreamJson);
    context.secrets = secrets(&[("GEMINI_API_KEY", "g-secret")]);
    context.mcp_servers = vec![server("docs"), server("search")];
    context.instructions = Some(Instructions {
        text: "Use the docs server.".into(),
        plugin_dir: None,
    });
    context.model = Some("Gemini 3.8 Flash (Medium)".into());
    let plan = provision(&temp, &context);
    golden("antigravity-mcp", &temp, &plan);
    assert!(plan.session.mcp_servers.is_empty() && plan.session.instructions.is_none());
    assert_eq!(
        plan.session.model.as_deref(),
        Some("Gemini 3.8 Flash (Medium)")
    );

    // A later turn without `search` and without instructions removes only
    // what Branchyard installed.
    context.installed = installed(&temp.home());
    assert_eq!(context.installed.mcp_servers, ["docs", "search"]);
    context.mcp_servers = vec![server("docs")];
    context.instructions = None;
    let plan = provision(&temp, &context);
    golden("antigravity-mcp-later", &temp, &plan);
}

#[test]
fn a_home_that_is_yours_gets_no_files_and_no_secrets() {
    let mut context = context("codex", Protocol::CodexAppServer);
    context.private_home = false;
    context.sandbox = false;
    context.secrets = secrets(&[("OPENAI_API_KEY", "sk-x")]);
    let refused = branchyard_provision::plan(&context).unwrap_err().0;
    assert!(refused.contains("--isolated") && !refused.contains("sk-x"));
    context.secrets.clear();
    context.effort = Some(Effort::Low);
    assert!(branchyard_provision::plan(&context)
        .unwrap_err()
        .0
        .contains("--isolated"));
    let mut context = self::context("antigravity", Protocol::AntigravityStreamJson);
    context.private_home = false;
    context.mcp_servers = vec![server("docs")];
    assert!(branchyard_provision::plan(&context).is_err());
    // A session channel needs no home at all.
    let mut context = self::context("claude-code", Protocol::ClaudeStreamJson);
    context.private_home = false;
    context.mcp_servers = vec![server("docs")];
    context.model = Some("sonnet".into());
    let planned = branchyard_provision::plan(&context).unwrap();
    assert!(planned.files.is_empty());
    assert_eq!(planned.session.mcp_servers, context.mcp_servers);
}

#[test]
fn links_in_the_home_are_never_followed() {
    let temp = Temp::new();
    let outside = temp.0.join("worktree");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("auth.json"), "tracked file\n").unwrap();
    let mut context = context("codex", Protocol::CodexAppServer);
    context.secrets = secrets(&[("OPENAI_API_KEY", "sk-link-secret")]);
    let plan = branchyard_provision::plan(&context).unwrap();

    // A linked directory on the way is refused.
    symlink(&outside, temp.home().join(".codex")).unwrap();
    let error = apply(&plan, &temp.home()).unwrap_err().to_string();
    assert!(
        error.contains("symbolic link") && !error.contains("sk-link-secret"),
        "{error}"
    );
    fs::remove_file(temp.home().join(".codex")).unwrap();

    // A linked file is replaced, not written through.
    fs::create_dir_all(temp.home().join(".codex")).unwrap();
    symlink(
        outside.join("auth.json"),
        temp.home().join(".codex/auth.json"),
    )
    .unwrap();
    apply(&plan, &temp.home()).unwrap();
    assert_eq!(
        fs::read_to_string(outside.join("auth.json")).unwrap(),
        "tracked file\n"
    );
    assert!(!fs::symlink_metadata(temp.home().join(".codex/auth.json"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(temp.read(".codex/auth.json").contains("sk-link-secret"));
}

#[test]
fn an_unparseable_user_file_is_refused_and_kept() {
    let temp = Temp::new();
    temp.seed(".gemini/settings.json", "{ this is not json");
    let mut context = context("gemini-cli", Protocol::Acp);
    context.model = Some("m".into());
    let error = apply(&branchyard_provision::plan(&context).unwrap(), &temp.home()).unwrap_err();
    assert!(error.to_string().contains("not valid JSON"));
    assert_eq!(temp.read(".gemini/settings.json"), "{ this is not json");
}

/// Everything a plan writes or sets, as text.
fn everything(plan: &Plan) -> String {
    format!(
        "{:?} {:?} {:?}",
        plan.files
            .iter()
            .map(|f| format!("{} {:?}", f.path, f.edits))
            .collect::<Vec<_>>(),
        plan.env
            .iter()
            .map(|e| format!("{}={}", e.name, e.value))
            .collect::<Vec<_>>(),
        plan.session
    )
}

#[test]
fn no_plan_turns_approvals_or_the_sandbox_off() {
    // Scion launches Claude Code with --dangerously-skip-permissions and
    // Codex with --dangerously-bypass-approvals-and-sandbox, and seeds
    // approval_policy = "never", "yolo": true and HERMES_YOLO_MODE.
    // Branchyard routes every approval; no provisioned setting may undo it.
    let forbidden = [
        "bypasspermissions",
        "defaultmode",
        "approval_policy",
        "sandbox_mode",
        "danger",
        "yolo",
        "skip-permissions",
        "accept_hooks",
    ];
    let every_secret = secrets(&[
        ("ANTHROPIC_API_KEY", "a"),
        ("OPENAI_API_KEY", "b"),
        ("GEMINI_API_KEY", "c"),
        ("GH_TOKEN", "d"),
    ]);
    for provisioner in provisioners() {
        for protocol in [
            Protocol::ClaudeStreamJson,
            Protocol::CodexAppServer,
            Protocol::Acp,
            Protocol::AntigravityStreamJson,
        ] {
            let mut context = context(provisioner.harness(), protocol);
            context.secrets = every_secret.clone();
            context.mcp_servers = vec![server("docs")];
            context.instructions = Some(Instructions {
                text: "rules".into(),
                plugin_dir: None,
            });
            for (model, effort, telemetry) in [
                (None, None, None),
                (Some("m".to_owned()), None, None),
                (None, Some(Effort::High), None),
                (None, None, self::telemetry("http://127.0.0.1:4317")),
            ] {
                context.model = model;
                context.effort = effort;
                context.telemetry = telemetry;
                if let Ok(plan) = branchyard_provision::plan(&context) {
                    let text = everything(&plan).to_lowercase();
                    for word in forbidden {
                        assert!(
                            !text.contains(word),
                            "{} plans {word}: {text}",
                            provisioner.harness()
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn every_scion_provisioner_is_translated_or_explained() {
    use branchyard_controls::harness;
    let vendored: Vec<String> =
        fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor/scion/harnesses"))
            .unwrap()
            .map(|e| e.unwrap())
            .filter(|e| e.path().join("provision.py").is_file())
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
    assert_eq!(vendored.len(), 9);
    for directory in &vendored {
        let ported = provisioners().iter().find(|p| p.scion() == Some(directory));
        let explained = NOT_PORTED.iter().any(|(d, _)| d == directory);
        assert!(ported.is_some() != explained, "{directory}");
        let registered = harness::from_scion(directory).expect("mapped in the registry");
        match ported {
            Some(p) => {
                assert_eq!(p.harness(), registered.id);
                assert!(
                    branchyard_harness::profiles::default_for(p.harness()).is_some(),
                    "{} has no profile",
                    p.harness()
                );
            }
            None => assert!(branchyard_harness::profiles::default_for(registered.id).is_none()),
        }
    }
    assert_eq!(for_harness("pi").harness(), "*");
}

#[test]
fn plans_never_show_secret_values_in_debug_output() {
    let mut context = context("codex", Protocol::CodexAppServer);
    context.secrets = secrets(&[("CODEX_API_KEY", "sk-debug-secret")]);
    let plan = branchyard_provision::plan(&context).unwrap();
    assert!(!format!("{plan:?} {context:?}").contains("sk-debug-secret"));
}
