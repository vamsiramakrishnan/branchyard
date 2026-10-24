//! `by init plugin`'s install engine over a fixture plugin directory that
//! uses every `how` (tests/fixtures/plugin), and over the manifests built
//! into `by` (plugins/branchyard/install).

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
use std::collections::BTreeMap;

use branchyard_setup::install::{self, Bundle, How};
use branchyard_setup::probe::FakeProbe;
use branchyard_setup::{
    next, plan::FileAction, BuiltinValidator, Facts, Plan, PlannedFile, Topic, Validator,
};
use serde_json::{json, Value};

macro_rules! fixture {
    ($path:literal) => {
        ($path, include_str!(concat!("fixtures/plugin/", $path)))
    };
}

const FIXTURE: Bundle<'static> = Bundle(&[
    fixture!("harnesses/fixture/config.toml"),
    fixture!("harnesses/fixture/config.yaml"),
    fixture!("harnesses/fixture/ext/commands/status.md"),
    fixture!("harnesses/fixture/ext/manifest.json"),
    fixture!("harnesses/fixture/hook.sh"),
    fixture!("harnesses/fixture/settings.json"),
    fixture!("harnesses/fixture/statusline.json"),
    fixture!("install/fixture.toml"),
]);

const HOME: &str = "/home/dev";

fn plan_with(probe: &FakeProbe) -> Plan {
    let facts = Facts::gather(probe);
    let manifests = FIXTURE.manifests().unwrap();
    let mut plan = Plan::new(Topic::Plugin);
    plan.files = install::planned_files_from(FIXTURE, &manifests, &facts, probe);
    plan.validate(&BuiltinValidator);
    plan
}

fn file<'a>(plan: &'a Plan, path: &str) -> &'a PlannedFile {
    plan.files
        .iter()
        .find(|f| f.path == path)
        .unwrap_or_else(|| panic!("{path} not planned: {:#?}", plan.files))
}

/// The person's files, as they were before the install.
fn existing() -> BTreeMap<String, String> {
    [
        (
            format!("{HOME}/.fixture/settings.json"),
            "{\n    \"theme\": \"dark\",\n    \"hooks\": {\n        \"sessionStart\": [{\"command\": \"mine\"}]\n    }\n}\n".to_owned(),
        ),
        (
            format!("{HOME}/.fixture/config.toml"),
            "# mine\nmodel = \"m\" # keep this comment\n\n[features]\nother = 1\n".to_owned(),
        ),
        (
            format!("{HOME}/.fixture/config.yaml"),
            "# mine\nmodel: m\nhooks:\n  pre_llm_call:\n    - command: mine\n".to_owned(),
        ),
    ]
    .into()
}

#[test]
fn every_how_plans_a_diffed_validated_file_and_a_second_run_changes_nothing() {
    let mut probe = FakeProbe::typical();
    probe.files.extend(existing());
    let plan = plan_with(&probe);
    assert!(plan.valid, "{:#?}", plan.files);
    let paths: Vec<&str> = plan.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "/home/dev/.fixture/hooks/branchyard.sh",
            // {project}: relative to the repository root.
            ".fixture/extensions/branchyard/commands/status.md",
            ".fixture/extensions/branchyard/manifest.json",
            "/home/dev/.fixture/settings.json",
            "/home/dev/.fixture/config.toml",
            "/home/dev/.fixture/config.yaml",
        ]
    );
    let script = file(&plan, "/home/dev/.fixture/hooks/branchyard.sh");
    assert_eq!(
        (script.action, script.mode.as_str()),
        (FileAction::Create, "0755")
    );
    assert_eq!(script.how.as_deref(), Some("write"));

    // Two fragments into one JSON file: both merged, the person's key
    // order, indentation and hook kept, nothing counted as replacing it.
    let settings = file(&plan, "/home/dev/.fixture/settings.json");
    assert_eq!(settings.action, FileAction::Update);
    assert!(!settings.overwrites);
    let merged: Value = serde_json::from_str(settings.content.as_deref().unwrap()).unwrap();
    assert_eq!(merged["theme"], "dark");
    assert_eq!(merged["hooks"]["sessionStart"].as_array().unwrap().len(), 2);
    assert_eq!(merged["hooks"]["sessionStart"][0]["command"], "mine");
    assert_eq!(
        merged["ui"]["statusLine"]["command"],
        "by statusline --harness fixture"
    );
    assert!(settings
        .content
        .as_deref()
        .unwrap()
        .starts_with("{\n    \"theme\""));
    assert!(settings.diff.as_deref().unwrap().contains("+    \"ui\": {"));
    assert_eq!(settings.validation.validator, "json");

    let config = file(&plan, "/home/dev/.fixture/config.toml");
    let text = config.content.as_deref().unwrap();
    assert!(
        text.starts_with("# mine\nmodel = \"m\" # keep this comment\n"),
        "{text}"
    );
    assert!(
        text.contains("other = 1") && text.contains("hooks = true"),
        "{text}"
    );
    assert_eq!(config.how.as_deref(), Some("merge-toml"));

    let yaml = file(&plan, "/home/dev/.fixture/config.yaml");
    assert_eq!(
        yaml.content.as_deref().unwrap(),
        "# mine\nmodel: m\nhooks:\n  pre_llm_call:\n    - command: mine\n  on_session_start:\n    - command: \"command -v by >/dev/null 2>&1 && by hook --harness fixture session-start || true\"\n      timeout: 5\n"
    );
    assert!(yaml.validation.skipped);

    // Apply it, then plan again: every file is unchanged.
    let mut after = probe.clone();
    for f in &plan.files {
        after.files.insert(f.path.clone(), f.body().to_owned());
    }
    let again = plan_with(&after);
    for f in &again.files {
        assert_eq!(f.action, FileAction::Unchanged, "{}: {:?}", f.path, f.diff);
    }
}

#[test]
fn a_clash_refuses_the_plan_and_a_different_written_file_needs_consent() {
    let mut probe = FakeProbe::typical();
    probe.files.insert(
        format!("{HOME}/.fixture/config.toml"),
        "[features]\nhooks = false\n".into(),
    );
    probe.files.insert(
        format!("{HOME}/.fixture/hooks/branchyard.sh"),
        "#!/bin/sh\necho mine\n".into(),
    );
    let plan = plan_with(&probe);
    assert!(!plan.valid);
    let refused = file(&plan, "/home/dev/.fixture/config.toml");
    assert!(!refused.writes() && refused.content.is_none());
    let why = refused.validation.messages.join("; ");
    assert!(why.contains("`features.hooks` is already set"), "{why}");
    let script = file(&plan, "/home/dev/.fixture/hooks/branchyard.sh");
    assert!(
        script.overwrites,
        "a different written file is a replacement"
    );
    assert!(script.diff.as_deref().unwrap().contains("-echo mine"));
}

#[test]
fn detection_is_any_binary_on_path() {
    let mut probe = FakeProbe::typical();
    let manifests = FIXTURE.manifests().unwrap();
    assert_eq!(manifests[0].files[0].how, How::Write);
    assert!(!manifests[0]
        .binaries
        .iter()
        .any(|b| probe.tools.contains_key(b)));
    probe.tools.insert("fx".into(), String::new());
    assert!(manifests[0].binaries.iter().any(|b| {
        use branchyard_setup::Probe;
        probe.on_path(b)
    }));
}

#[test]
fn a_manifest_naming_a_missing_file_or_a_bad_target_is_refused() {
    let bad = Bundle(&[(
        "install/bad.toml",
        "harness = \"other\"\nbinaries = []\n[[file]]\nfrom = \"nope.json\"\nto = \"relative/x.json\"\nhow = \"write\"\nat = \"x\"\n",
    )]);
    let e = bad.manifests().unwrap_err();
    for wanted in [
        "named for \"bad\"",
        "binaries must name",
        "nope.json: no such file",
        "`to` must start with",
        "`at` is only for merge-*",
    ] {
        assert!(e.contains(wanted), "{wanted}: {e}");
    }
    let unknown = Bundle(&[(
        "install/x.toml",
        "harness = \"x\"\nbinaries = [\"x\"]\nextra = 1\n",
    )]);
    assert!(unknown.manifests().unwrap_err().contains("unknown field"));
}

#[test]
fn the_built_manifests_are_valid_and_claude_codes_hooks_are_the_plugins() {
    let manifests = install::manifests().unwrap();
    let claude = manifests
        .iter()
        .find(|m| m.harness == "claude-code")
        .expect("install/claude-code.toml");
    assert_eq!(claude.binaries, ["claude"]);
    // The settings fragment runs exactly the plugin's hooks.
    let plugin: Value =
        serde_json::from_str(install::BUILT.file("hooks/hooks.json").unwrap()).unwrap();
    let fragment: Value = serde_json::from_str(
        install::BUILT
            .file("harnesses/claude-code/settings-hooks.json")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(plugin["hooks"], fragment["hooks"]);
    // Every built manifest plans validly into an empty home.
    let probe = FakeProbe::typical();
    let facts = Facts::gather(&probe);
    let mut plan = Plan::new(Topic::Plugin);
    plan.files = install::planned_files(&manifests, &facts, &probe);
    plan.validate(&BuiltinValidator);
    for f in &plan.files {
        assert!(f.validation.ok, "{}: {:?}", f.path, f.validation);
        assert!(BuiltinValidator.validate(f, &plan.files).ok);
    }
}

/// Every text file under `dir`, by path relative to it.
fn read_tree(dir: &std::path::Path, out: &mut BTreeMap<String, String>) {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(text) = std::fs::read_to_string(&path) {
                let relative = path
                    .strip_prefix(dir)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert(relative, text);
            }
        }
    }
}

/// The manifests on disk, or in the union of the plugin directories in
/// `BRANCHYARD_PLUGIN_DIRS` (colon-separated; to check other branches'
/// manifests before they merge): each parses, plans validly into an empty
/// home, and plans nothing more once applied.
#[test]
fn the_manifests_on_disk_plan_validly_and_idempotently() {
    let dirs = std::env::var("BRANCHYARD_PLUGIN_DIRS").unwrap_or_else(|_| {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../plugins/branchyard").to_owned()
    });
    let mut tree = BTreeMap::new();
    for dir in dirs.split(':').filter(|d| !d.is_empty()) {
        read_tree(std::path::Path::new(dir), &mut tree);
    }
    let files: Vec<(&str, &str)> = tree.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
    let bundle = Bundle(&files);
    let manifests = bundle.manifests().unwrap();
    assert!(manifests.iter().any(|m| m.harness == "claude-code"));
    let probe = FakeProbe::typical();
    let facts = Facts::gather(&probe);
    let mut plan = Plan::new(Topic::Plugin);
    plan.files = install::planned_files_from(bundle, &manifests, &facts, &probe);
    plan.validate(&BuiltinValidator);
    assert!(
        plan.valid,
        "{:#?}",
        plan.files
            .iter()
            .filter(|f| !f.validation.ok)
            .collect::<Vec<_>>()
    );
    let mut after = probe.clone();
    for f in &plan.files {
        after.files.insert(f.path.clone(), f.body().to_owned());
    }
    let again = install::planned_files_from(bundle, &manifests, &facts, &after);
    for f in &again {
        assert_eq!(f.action, FileAction::Unchanged, "{}: {:?}", f.path, f.diff);
    }
    eprintln!(
        "{} manifests, {} files: {:?}",
        manifests.len(),
        plan.files.len(),
        manifests
            .iter()
            .map(|m| m.harness.as_str())
            .collect::<Vec<_>>()
    );
}

#[test]
fn by_init_plugin_offers_detected_harnesses_and_plans_them() {
    let mut probe = FakeProbe::typical();
    probe.tools.insert("claude".into(), "2.1.283".into());
    let facts = Facts::gather(&probe);
    assert!(facts.install_detected.contains(&"claude-code".to_owned()));
    let list = install::listing(&facts).unwrap();
    assert_eq!(list.harnesses[0].harness, "claude-code");
    assert!(list.harnesses[0].detected);
    let answers: BTreeMap<String, Value> = [
        ("host".to_owned(), json!("harnesses")),
        ("harnesses".to_owned(), json!(["claude-code"])),
    ]
    .into();
    let response = next(
        Topic::Plugin,
        &probe,
        &answers,
        false,
        &mut branchyard_setup::probe::CountingEntropy(0),
        &BuiltinValidator,
    );
    assert!(response.done, "{:?}", response.questions);
    let plan = response.plan.unwrap();
    assert!(plan.valid, "{:#?}", plan.files);
    let settings = file(&plan, "/home/dev/.claude/settings.json");
    assert_eq!(settings.how.as_deref(), Some("merge-json"));
    assert!(plan
        .files
        .iter()
        .any(|f| f.path == "/home/dev/.claude/skills/setup/SKILL.md"));
    assert!(
        plan.summary[0].starts_with("Claude Code: hooks"),
        "{:?}",
        plan.summary
    );
    assert!(plan.notes.iter().any(|n| n.contains("marketplace")));
    // Asked by default: the detected ones.
    let ask: BTreeMap<String, Value> = [("host".to_owned(), json!("harnesses"))].into();
    let asked = next(
        Topic::Plugin,
        &probe,
        &ask,
        false,
        &mut branchyard_setup::probe::CountingEntropy(0),
        &BuiltinValidator,
    );
    let q = asked
        .questions
        .iter()
        .find(|q| q.id == "harnesses")
        .unwrap();
    assert_eq!(q.default, json!(["claude-code"]));
    assert_eq!(q.choices[0].value, json!("claude-code"));
    // An unknown harness is refused by name.
    let wrong: BTreeMap<String, Value> = [
        ("host".to_owned(), json!("harnesses")),
        ("harnesses".to_owned(), json!(["nope"])),
    ]
    .into();
    let refused = next(
        Topic::Plugin,
        &probe,
        &wrong,
        false,
        &mut branchyard_setup::probe::CountingEntropy(0),
        &BuiltinValidator,
    );
    assert!(!refused.errors.is_empty());
}
