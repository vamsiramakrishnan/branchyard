//! Harness inventories (docs/harness-lifecycle.md) with fake harness
//! binaries on a temporary `PATH`: detection (logged in, logged out and
//! unknown; missing; a version command that hangs; a key variable seen by
//! name only), the cache, an install through a fake `npm` that records its
//! calls and is verified by detecting again, the log, and the router
//! excluding what a machine cannot run. Nothing is installed from the
//! network. Requires `sh`.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use branchyard::inventory::{
    self, DetectOptions, Evidence, HarnessEvent, HarnessLog, InstallAction, InstallMode,
    InstallPolicy, Inventory, InventoryCache, LocalGate, LoginState,
};
use branchyard::{Fleet, FleetCandidate, FleetEntry, RouteOptions, TaskOptions};
use common::{fake_agent, Fixture};

struct Machine {
    _dir: tempfile::TempDir,
    bin: PathBuf,
    tools: PathBuf,
    home: PathBuf,
    log: PathBuf,
}

impl Machine {
    fn new() -> Machine {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let (bin, home) = (base.join("bin"), base.join("home"));
        fs::create_dir_all(&bin).unwrap();
        let tools = base.join("tools");
        branchyard_testkit::system_tools(&tools).unwrap();
        fs::create_dir_all(home.join(".local/bin")).unwrap();
        Machine {
            log: base.join("calls.log"),
            _dir: dir,
            bin,
            tools,
            home,
        }
    }

    fn fake(&self, name: &str, body: &str) {
        let path = self.bin.join(name);
        fs::write(
            &path,
            format!(
                "#!/bin/sh\necho \"{name} $*\" >> '{}'\n{body}",
                self.log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// `sh` with this machine's PATH and HOME and nothing else.
    fn shell(&self) -> Command {
        let mut command = Command::new("/bin/sh");
        command
            .env_clear()
            .env(
                "PATH",
                format!(
                    "{}:{}:{}",
                    self.bin.display(),
                    self.home.join(".local/bin").display(),
                    self.tools.display()
                ),
            )
            .env("HOME", &self.home);
        command
    }

    fn detect(&self, options: &DetectOptions, vars: &[(&str, &str)]) -> Inventory {
        inventory::detect_with(options, true, |_| {
            let mut command = self.shell();
            command.arg("-s");
            for (k, v) in vars {
                command.env(k, v);
            }
            command
        })
        .unwrap()
    }
}

fn options() -> DetectOptions {
    DetectOptions {
        timeout: Duration::from_millis(500),
        dirs: vec!["$HOME/.local/bin".into()],
        only: None,
    }
}

#[test]
fn detection_reads_versions_and_logins_and_times_out_slow_commands() {
    let m = Machine::new();
    m.fake(
        "codex",
        "case \"$1\" in --version) echo 'codex-cli 0.157.1';; login) echo 'Not logged in';; esac\n",
    );
    m.fake("goose", "echo '1.9.0'\n");
    m.fake("qwen", "exec sleep 30\n");
    m.fake("claude", "echo '2.1.283 (Claude Code)'\n");
    let found = m.detect(&options(), &[("ANTHROPIC_API_KEY", "sk-never-printed")]);
    let ids: Vec<&str> = found.harnesses.iter().map(|h| h.id.as_str()).collect();
    assert_eq!(ids, ["claude-code", "codex", "goose", "qwen-code"]);
    assert!(found.checked.iter().any(|c| c == "opencode"));
    assert!(found.get("opencode").is_none());

    let codex = found.get("codex").unwrap();
    assert_eq!(codex.version.as_deref(), Some("0.157.1"));
    assert_eq!(codex.login.state, LoginState::LoggedOut);
    assert_eq!(codex.login.evidence, Evidence::Verified);
    assert!(found
        .ready("codex")
        .unwrap_err()
        .contains("verified logged out"));
    let claude = found.get("claude-code").unwrap();
    assert_eq!(claude.login.state, LoginState::LoggedIn);
    assert_eq!(claude.login.evidence, Evidence::Likely);
    assert_eq!(
        claude.login.detail,
        "ANTHROPIC_API_KEY is set (an API key; not checked)"
    );
    let goose = found.get("goose").unwrap();
    assert_eq!(goose.version.as_deref(), Some("1.9.0"));
    assert_eq!(goose.login.state, LoginState::Unknown);
    let qwen = found.get("qwen-code").unwrap();
    assert_eq!(qwen.version, None);
    assert_eq!(
        qwen.version_note.as_deref(),
        Some("slow to start (took >0.5 s)")
    );
    assert_eq!(qwen.version_text(), "slow to start");
    assert!(found
        .ready("opencode")
        .unwrap_err()
        .contains("not installed"));
    assert_eq!(
        found.labels(),
        ["harness:claude-code", "harness:goose", "harness:qwen-code"]
    );
    // Nothing secret went anywhere: the inventory names the variable only.
    assert!(!serde_json::to_string(&found)
        .unwrap()
        .contains("sk-never-printed"));
    // Each version command ran once, and each status command once (Claude
    // Code's and Codex's own).
    let mut calls = m.calls();
    calls.sort();
    assert_eq!(
        calls,
        [
            "claude --version",
            "claude auth status",
            "codex --version",
            "codex login status",
            "goose --version",
            "qwen --version",
        ]
    );

    // Only some harnesses: the rest are not looked for.
    let some = m.detect(
        &DetectOptions {
            only: Some(vec!["goose".into()]),
            ..options()
        },
        &[],
    );
    assert_eq!(some.checked, ["goose"]);
    assert!(some.ready("codex").unwrap_err().contains("not looked for"));
}

#[test]
fn antigravity_deepseek_and_gemini_logins_are_read_from_their_configuration() {
    // agy 1.3.3 uses GEMINI_API_KEY only with modelProvider gemini in its
    // settings; dsh 0.2.0-rc.2 takes a provider from cordis.patch.yml; gemini
    // 0.63.0 headless needs the workspace trusted.
    let m = Machine::new();
    m.fake("agy", "echo '1.3.3'\n");
    m.fake("dsh", "echo '0.2.0-rc.2'\n");
    m.fake("gemini", "echo '0.63.0'\n");
    let only = DetectOptions {
        only: Some(vec![
            "antigravity".into(),
            "deepseek-harness".into(),
            "gemini-cli".into(),
        ]),
        ..options()
    };
    let secret = "g-never-printed";

    // Keys and nothing configured.
    let found = m.detect(&only, &[("GEMINI_API_KEY", secret)]);
    let agy = found.get("antigravity").unwrap();
    assert_eq!(agy.version.as_deref(), Some("1.3.3"));
    assert_eq!(agy.login.state, LoginState::LoggedIn);
    assert_eq!(agy.login.evidence, Evidence::Likely);
    assert_eq!(
        agy.login.detail,
        "GEMINI_API_KEY is set but ~/.gemini/antigravity-cli/settings.json does not set \
         modelProvider gemini: agy will ask for a Google sign-in; Branchyard sets it in \
         isolated runs, and `by harnesses login antigravity` sets it for your own"
    );
    assert_eq!(agy.login.key.as_deref(), Some("GEMINI_API_KEY"));
    assert_eq!(agy.login.label(), "key set, unused");
    assert!(agy.login.key_unused());
    let gemini = found.get("gemini-cli").unwrap();
    assert_eq!(gemini.version.as_deref(), Some("0.63.0"));
    assert!(gemini
        .login
        .detail
        .starts_with("GEMINI_API_KEY is set (an API key; not checked); headless runs"));
    assert!(gemini
        .login
        .detail
        .contains("GEMINI_CLI_TRUST_WORKSPACE=true"));
    let dsh = found.get("deepseek-harness").unwrap();
    assert_eq!(dsh.version.as_deref(), Some("0.2.0-rc.2"));
    assert_eq!(dsh.login.state, LoginState::LoggedOut);
    assert_eq!(dsh.login.evidence, Evidence::Likely);
    assert!(dsh.login.detail.contains("DEEPSEEK_API_KEY"));
    assert!(found
        .ready("deepseek-harness")
        .unwrap_err()
        .contains("likely logged out"));
    // A key alone never reaches the report.
    assert!(!serde_json::to_string(&found).unwrap().contains(secret));

    // Settings that select the gemini provider, and a trusted workspace.
    let settings = m.home.join(".gemini/antigravity-cli");
    fs::create_dir_all(&settings).unwrap();
    fs::write(
        settings.join("settings.json"),
        "{\n  \"permissions\": {\"allow\": [\"command(git)\"]},\n  \"modelProvider\": \"gemini\"\n}\n",
    )
    .unwrap();
    let found = m.detect(
        &only,
        &[
            ("GEMINI_API_KEY", secret),
            ("GEMINI_CLI_TRUST_WORKSPACE", "true"),
        ],
    );
    assert_eq!(
        found.get("antigravity").unwrap().login.detail,
        "GEMINI_API_KEY is set and settings.json selects the gemini provider \
         (an API key; not checked)"
    );
    assert!(found
        .get("gemini-cli")
        .unwrap()
        .login
        .detail
        .ends_with("GEMINI_CLI_TRUST_WORKSPACE=true is set, so headless runs trust the workspace"));
    // Another provider in the settings does not count, and neither does a
    // key the variable name misses.
    fs::write(
        settings.join("settings.json"),
        "{\"modelProvider\": \"vertex\"}\n",
    )
    .unwrap();
    let found = m.detect(&only, &[("GOOGLE_API_KEY", secret)]);
    let agy = found.get("antigravity").unwrap();
    assert_eq!(agy.login.state, LoginState::LoggedOut, "{:?}", agy.login);
    assert!(!agy.login.detail.contains("GOOGLE_API_KEY"));
    assert_eq!(
        found.get("gemini-cli").unwrap().login.state,
        LoginState::LoggedIn,
        "gemini reads GOOGLE_API_KEY"
    );

    // DeepSeek Harness: a provider in either config file, an apiKeyEnv whose
    // variable is set, the shipped provider's key; not a comment, not an
    // apiKeyEnv whose variable is unset, not another DSH_HOME.
    let dsh_home = m.home.join(".dsh");
    fs::create_dir_all(dsh_home.join("profiles/acp")).unwrap();
    let detail = |m: &Machine, vars: &[(&str, &str)]| -> (LoginState, String) {
        let found = m.detect(&only, vars);
        let login = &found.get("deepseek-harness").unwrap().login;
        (login.state, login.detail.clone())
    };
    fs::write(
        dsh_home.join("cordis.patch.yml"),
        "# providers:\n#   apiKeyEnv: COMMENTED_KEY\nplugins: []\n",
    )
    .unwrap();
    assert_eq!(
        detail(&m, &[("COMMENTED_KEY", "x")]).0,
        LoginState::LoggedOut,
        "a comment configures nothing"
    );
    fs::write(
        dsh_home.join("profiles/acp/cordis.patch.yml"),
        "plugins:\n  - id: llm-pi-ai\n    config:\n      providers:\n        proxy:\n          \
         apiKeyEnv: PROXY_KEY\n          api: openai-completions\n",
    )
    .unwrap();
    // The provider names PROXY_KEY and it is unset: dsh stops with
    // MISSING_CREDENTIAL, so this is not a login (seen on dsh 0.2.1-rc.2).
    assert_eq!(
        detail(&m, &[]),
        (
            LoginState::LoggedOut,
            "~/.dsh/profiles/acp/cordis.patch.yml configures a provider, but the variable its \
             apiKeyEnv names (PROXY_KEY) is not set, and dsh stops with MISSING_CREDENTIAL; \
             export it"
                .to_owned()
        )
    );
    assert_eq!(
        detail(&m, &[("PROXY_KEY", secret)]),
        (
            LoginState::LoggedIn,
            "PROXY_KEY is set (an API key; not checked)".to_owned()
        )
    );
    fs::write(
        dsh_home.join("profiles/acp/cordis.patch.yml"),
        "plugins:\n  - id: agent-default-model\n    config:\n      provider: deepseek-official\n",
    )
    .unwrap();
    assert_eq!(detail(&m, &[]).0, LoginState::LoggedOut);
    assert_eq!(
        detail(&m, &[("DEEPSEEK_API_KEY", secret)]),
        (
            LoginState::LoggedIn,
            "DEEPSEEK_API_KEY is set (an API key; not checked)".to_owned()
        )
    );
    fs::write(
        dsh_home.join("cordis.patch.yml"),
        "plugins:\n  - id: llm-pi-ai\n    config:\n      providers:\n        mine: {}\n",
    )
    .unwrap();
    assert_eq!(
        detail(&m, &[]).1,
        "~/.dsh/cordis.patch.yml configures a provider (its key variable is not checked)"
    );
    let elsewhere = m.home.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    let elsewhere = elsewhere.to_str().unwrap();
    assert_eq!(
        detail(&m, &[("DSH_HOME", elsewhere)]).0,
        LoginState::LoggedOut,
        "DSH_HOME moves the configuration"
    );
    fs::write(
        m.home.join("elsewhere/cordis.patch.yml"),
        "providers:\n  moved:\n    apiKeyEnv: 'MOVED_KEY'\n",
    )
    .unwrap();
    assert_eq!(
        detail(&m, &[("DSH_HOME", elsewhere), ("MOVED_KEY", secret)]).1,
        "MOVED_KEY is set (an API key; not checked)"
    );
}

/// Hermes Agent 0.19.0 loads `~/.hermes/.env` for its providers' keys
/// (`HERMES_HOME` moves it): a `NAME_API_KEY=value` line is a login, by
/// name only, and an empty value, a comment and a name that is no key are
/// not. Checked against a Hermes configured for Z.ai's `zai` provider on
/// 25 October 2026.
#[test]
fn hermes_is_logged_in_by_a_key_in_its_env_file_never_printing_the_value() {
    let m = Machine::new();
    m.fake("hermes", "echo 'Hermes Agent v0.19.0 (2026.7.20)'\n");
    let only = DetectOptions {
        only: Some(vec!["hermes".into()]),
        ..options()
    };
    let secret = "h-never-printed";
    let login = |m: &Machine, vars: &[(&str, &str)]| {
        let found = m.detect(&only, vars);
        let hermes = found.get("hermes").unwrap().clone();
        let text = serde_json::to_string(&found).unwrap();
        assert!(!text.contains(secret), "a key value reached the report");
        (hermes.login.state, hermes.login.detail)
    };
    assert_eq!(
        login(&m, &[]).0,
        LoginState::Unknown,
        "no file, no variable"
    );
    let env_file = m.home.join(".hermes");
    fs::create_dir_all(&env_file).unwrap();
    fs::write(
        env_file.join(".env"),
        format!("# GLM_API_KEY={secret}\nTERMINAL_ENV=local\nOPENROUTER_API_KEY=\n"),
    )
    .unwrap();
    assert_eq!(
        login(&m, &[]).0,
        LoginState::Unknown,
        "a comment, an empty key and a name that is no key configure nothing"
    );
    fs::write(
        env_file.join(".env"),
        format!("TERMINAL_ENV=local\nexport GLM_API_KEY={secret}\n"),
    )
    .unwrap();
    assert_eq!(
        login(&m, &[]),
        (
            LoginState::LoggedIn,
            "GLM_API_KEY is set in ~/.hermes/.env, which Hermes loads (an API key; not checked)"
                .to_owned()
        )
    );
    // HERMES_HOME moves the file.
    let elsewhere = m.home.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    let elsewhere = elsewhere.to_str().unwrap();
    assert_eq!(
        login(&m, &[("HERMES_HOME", elsewhere)]).0,
        LoginState::Unknown,
        "HERMES_HOME moves the configuration"
    );
}

#[test]
fn the_cache_keeps_an_inventory_for_its_ttl() {
    let dir = tempfile::tempdir().unwrap();
    let cache = InventoryCache::new(dir.path().join("inventory.json"), Duration::from_secs(60));
    let opts = options();
    assert_eq!(cache.get(&opts), None);
    let taken = Inventory {
        host: "box".into(),
        detected_at_ms: branchyard_support::time::now_ms(),
        checked: vec!["codex".into()],
        ..Inventory::default()
    };
    cache.put(&opts, &taken).unwrap();
    assert_eq!(cache.get(&opts), Some(taken.clone()));
    // A listing the cache answered says so, with its age.
    let fetched = cache.fetch(&opts, false).unwrap();
    assert_eq!(fetched.inventory, taken);
    assert!(fetched
        .cached
        .is_some_and(|age| age < Duration::from_secs(60)));
    assert_eq!(
        inventory::cached_note(Duration::from_millis(23_400)),
        "cached 23s ago; --refresh asks again"
    );
    assert_eq!(
        inventory::cached_note(Duration::from_secs(75)),
        "cached 1m 15s ago; --refresh asks again"
    );
    // Other directories are another machine's view.
    let other = DetectOptions {
        dirs: vec![],
        ..options()
    };
    assert_eq!(cache.get(&other), None);
    // Too old.
    let stale = Inventory {
        detected_at_ms: branchyard_support::time::now_ms() - 120_000,
        ..taken
    };
    cache.put(&opts, &stale).unwrap();
    assert_eq!(cache.get(&opts), None);
    cache.clear();
    assert!(!cache.path.exists());
}

#[test]
fn an_install_runs_the_pinned_command_and_is_verified_by_detecting_again() {
    let m = Machine::new();
    // A fake npm that installs a fake codex at the version asked for.
    m.fake(
        "npm",
        "v=${3##*@}\nprintf '#!/bin/sh\\necho codex-cli %s\\n' \"$v\" > \"$HOME/.local/bin/codex\"\n\
         chmod +x \"$HOME/.local/bin/codex\"\n",
    );
    let only = DetectOptions {
        only: Some(vec!["codex".into()]),
        ..options()
    };
    let before = m.detect(&only, &[]);
    assert!(before.get("codex").is_none());
    assert!(before.tools.contains_key("npm"));
    let plan = inventory::plan("codex", InstallAction::Install, None, &before.tools).unwrap();
    assert_eq!(plan.command, "npm install -g @openai/codex@0.157.1");
    let mut run = |command: &str| {
        let out = m.shell().arg("-c").arg(command).output().unwrap();
        Ok((
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        ))
    };
    let mut again = |_: &str| Ok(m.detect(&only, &[]));
    let done = inventory::install(&plan, None, &mut run, &mut again);
    assert!(done.verified, "{done:?}");
    assert_eq!(done.after.unwrap().version.as_deref(), Some("0.157.1"));
    assert_eq!(m.calls(), ["npm install -g @openai/codex@0.157.1"]);

    // A pin the install does not deliver is not verified.
    let pinned =
        inventory::plan("codex", InstallAction::Update, Some("0.2.0"), &before.tools).unwrap();
    let mut broken = |_: &str| Ok((Some(0), String::new()));
    let done = inventory::install(&pinned, None, &mut broken, &mut again);
    assert!(!done.verified);
    assert!(done.problem.unwrap().contains("not the pinned 0.2.0"));
    let mut failing = |_: &str| Ok((Some(1), "npm ERR! 404".to_owned()));
    let done = inventory::install(&pinned, None, &mut failing, &mut again);
    assert_eq!(done.problem.as_deref(), Some("npm exited with status 1"));
    assert!(done.output_tail.contains("404"));

    // The log keeps every event, in order, 0600.
    let log = HarnessLog::new(m.home.join("events.jsonl"));
    for outcome in ["verified", "failed"] {
        log.append(&HarnessEvent {
            at_ms: 1,
            on: "local".into(),
            harness: "codex".into(),
            action: InstallAction::Install,
            by: "test".into(),
            command: Some(plan.command.clone()),
            outcome: outcome.into(),
            version_before: None,
            version_after: None,
            detail: None,
        })
        .unwrap();
    }
    let read: Vec<String> = log.read().unwrap().into_iter().map(|e| e.outcome).collect();
    assert_eq!(read, ["verified", "failed"]);
    let mode = fs::metadata(&log.path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

fn by_name_fleet(names: &[&str]) -> Fleet {
    let mut candidates: Vec<FleetCandidate> =
        names.iter().map(|n| FleetCandidate::new(*n)).collect();
    let mut agent = FleetCandidate::new("gemini-cli");
    agent.command = Some(vec![fake_agent().display().to_string()]);
    candidates.push(agent);
    Fleet {
        entries: [(
            "default".to_owned(),
            FleetEntry {
                candidates,
                ..FleetEntry::default()
            },
        )]
        .into_iter()
        .collect(),
    }
}

#[test]
fn the_router_excludes_what_the_machine_cannot_run_with_the_reason() {
    let f = Fixture::new();
    let m = Machine::new();
    m.fake(
        "codex",
        "case \"$1\" in --version) echo 'codex-cli 0.157.1';; login) echo 'Not logged in';; esac\n",
    );
    let found = m.detect(&options(), &[]);
    let policy = InstallPolicy::new(Some(InstallMode::Never), vec![], false);
    let gate = LocalGate::new(found, policy, options()).into_arc();
    let how = RouteOptions {
        seed: Some(1),
        harnesses: Some(gate.clone()),
        ..RouteOptions::default()
    };
    let table = by_name_fleet(&["codex", "goose"]);
    let route = f
        .yard
        .route(
            "fix the bug",
            &TaskOptions::default(),
            &table,
            &how,
            Some(1),
        )
        .unwrap();
    assert_eq!(route.picks[0].candidate.harness, "gemini-cli");
    let reasons: Vec<(&str, &str)> = route
        .excluded
        .iter()
        .map(|e| (e.candidate.harness.as_str(), e.reason.as_str()))
        .collect();
    assert_eq!(reasons.len(), 2, "{reasons:?}");
    assert!(
        reasons[0].1.contains("codex is verified logged out"),
        "{reasons:?}"
    );
    assert!(
        reasons[1]
            .1
            .contains("goose is not installed on this machine; install it with"),
        "{reasons:?}"
    );
    // A candidate with a command of its own is checked as a path, not by
    // the inventory; so is every candidate of a task with a command.
    let commanded = TaskOptions {
        command: Some(vec![fake_agent().display().to_string()]),
        ..TaskOptions::default()
    };
    let route = f
        .yard
        .route("fix the bug", &commanded, &table, &how, Some(1))
        .unwrap();
    assert!(route.excluded.is_empty(), "{:?}", route.excluded);

    // Under "auto" with an allowlist that leaves it out, goose stays out.
    let found = m.detect(&options(), &[]);
    let auto = InstallPolicy::new(Some(InstallMode::Auto), vec!["codex".into()], false);
    let how = RouteOptions {
        seed: Some(1),
        harnesses: Some(Arc::new(LocalGate::new(found, auto, options()))),
        ..RouteOptions::default()
    };
    let route = f
        .yard
        .route(
            "fix the bug",
            &TaskOptions::default(),
            &by_name_fleet(&["goose"]),
            &how,
            Some(1),
        )
        .unwrap();
    assert!(
        route.excluded[0]
            .reason
            .contains("goose is not in [harnesses] allow (codex)"),
        "{:?}",
        route.excluded
    );
}

#[test]
fn a_gate_installs_on_demand_under_auto_and_records_it() {
    let m = Machine::new();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.env_clear();
    branchyard_testkit::hermetic(&mut command);
    let output = command
        .args([
            "gate_install_child",
            "--exact",
            "--ignored",
            "--test-threads=1",
        ])
        .env("PATH", &m.tools)
        .env("HOME", &m.home)
        .output()
        .unwrap();
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed;"),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "the parent test supplies a private process PATH and HOME"]
fn gate_install_child() {
    let m = Machine::new();
    let found = m.detect(
        &DetectOptions {
            only: Some(vec!["codex".into()]),
            ..options()
        },
        &[],
    );
    let ran = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = ran.clone();
    let log = HarnessLog::new(m.home.join("events.jsonl"));
    let mut tools = found.tools.clone();
    tools.insert("npm".into(), "/usr/bin/npm".into());
    let found = Inventory { tools, ..found };
    let gate = LocalGate::new(
        found,
        InstallPolicy::new(Some(InstallMode::Auto), vec![], false),
        options(),
    )
    .with_log(log.clone())
    .with_runner(move |command| {
        seen.lock().unwrap().push(command.to_owned());
        // Nothing appears on this process's PATH, so it is not verified.
        Ok((Some(0), String::new()))
    });
    use branchyard::inventory::HarnessGate;
    let why = gate.check("codex").unwrap_err();
    assert!(why.contains("installing it on demand failed"), "{why}");
    assert_eq!(
        *ran.lock().unwrap(),
        ["npm install -g @openai/codex@0.157.1"]
    );
    let events = log.read().unwrap();
    assert_eq!(events[0].by, "router");
    assert_eq!(events[0].outcome, "failed");
    // Still missing afterwards, and a preview says what a run would do
    // without running anything.
    assert!(gate.inventory().checked("codex"));
    assert!(gate.inventory().get("codex").is_none());
    let preview = LocalGate::new(
        gate.inventory(),
        InstallPolicy::new(Some(InstallMode::Auto), vec![], false),
        options(),
    )
    .with_runner(|_| panic!("a preview installs nothing"))
    .preview();
    let why = preview.check("codex").unwrap_err();
    assert!(
        why.contains("a routed run would install it first ([harnesses] install = \"auto\"): npm install -g @openai/codex@0.157.1"),
        "{why}"
    );
    // Under "ask" or "never" the router never installs.
    let ask = LocalGate::new(
        gate.inventory(),
        InstallPolicy::new(Some(InstallMode::Ask), vec![], true),
        options(),
    )
    .with_runner(|_| panic!("ask never installs on demand"));
    assert!(ask
        .check("codex")
        .unwrap_err()
        .contains("installs on demand only with [harnesses] install = \"auto\""));
    // A harness detection did not look for is left to the PATH check.
    assert_eq!(ask.check("goose"), Ok(()));
}
