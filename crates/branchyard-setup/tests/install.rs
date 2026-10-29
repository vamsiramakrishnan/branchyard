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
    // One line per harness: what it gets, and where.
    assert_eq!(
        plan.summary[0],
        format!(
            "Claude Code: {} skills, 3 commands, 2 hooks, status line in ~/.claude/settings.json",
            branchyard_setup::skills::SKILLS.len()
        ),
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

// ---- Claude Code's status line: merged when there is none, never replaced ----

/// Plan the built Claude Code install over the files in `existing`.
fn claude_plan(existing: &[(&str, &str)]) -> (Plan, Vec<String>) {
    let mut probe = FakeProbe::typical();
    probe.tools.insert("claude".into(), "2.1.310".into());
    for (path, text) in existing {
        probe
            .files
            .insert(format!("{HOME}/{path}"), (*text).to_owned());
    }
    let answers: BTreeMap<String, Value> =
        [("harnesses".to_owned(), json!(["claude-code"]))].into();
    let response = next(
        Topic::Plugin,
        &probe,
        &answers,
        false,
        &mut branchyard_setup::probe::CountingEntropy(0),
        &BuiltinValidator,
    );
    let plan = response.plan.unwrap();
    let notes = plan.notes.clone();
    (plan, notes)
}

fn settings(plan: &Plan) -> Value {
    let file = file(plan, "/home/dev/.claude/settings.json");
    serde_json::from_str(file.content.as_deref().unwrap()).unwrap()
}

#[test]
fn claude_codes_status_line_is_merged_when_there_is_none() {
    let (plan, notes) = claude_plan(&[]);
    assert!(plan.valid, "{:#?}", plan.files);
    let merged = settings(&plan);
    assert_eq!(merged["statusLine"]["type"], "command");
    let command = merged["statusLine"]["command"].as_str().unwrap();
    assert_eq!(
        command,
        "command -v by >/dev/null 2>&1 && by statusline --harness claude-code || true"
    );
    assert_eq!(merged["hooks"].as_object().unwrap().len(), 2);
    assert!(
        !notes.iter().any(|n| n.contains("kept the \"statusLine\"")),
        "{notes:?}"
    );

    // Their other settings stay; applied, a second plan changes nothing.
    let mine = "{\n  \"theme\": \"dark\"\n}\n";
    let (plan, _) = claude_plan(&[(".claude/settings.json", mine)]);
    let merged = settings(&plan);
    assert_eq!(merged["theme"], "dark");
    assert!(merged["statusLine"].is_object());
    let written = file(&plan, "/home/dev/.claude/settings.json")
        .body()
        .to_owned();
    let (again, notes) = claude_plan(&[(".claude/settings.json", written.as_str())]);
    for f in again
        .files
        .iter()
        .filter(|f| f.path.ends_with("settings.json"))
    {
        assert_eq!(f.action, FileAction::Unchanged, "{}: {:?}", f.path, f.diff);
    }
    assert!(
        !notes.iter().any(|n| n.contains("kept")),
        "installed already: {notes:?}"
    );
}

#[test]
fn a_status_line_you_already_have_is_kept_and_the_note_gives_the_line_to_choose() {
    let mine = r#"{
  "statusLine": {"type": "command", "command": "ccstatusline"},
  "theme": "dark"
}
"#;
    let (plan, notes) = claude_plan(&[(".claude/settings.json", mine)]);
    // Not a clash that refuses the install: the hooks are merged, the line is not.
    assert!(plan.valid, "{:#?}", plan.files);
    let merged = settings(&plan);
    assert_eq!(merged["statusLine"]["command"], "ccstatusline");
    assert_eq!(merged["theme"], "dark");
    assert_eq!(merged["hooks"].as_object().unwrap().len(), 2);
    let note = notes
        .iter()
        .find(|n| n.contains("kept the \"statusLine\""))
        .unwrap_or_else(|| panic!("{notes:?}"));
    assert!(
        note.starts_with(
            "Claude Code: kept the \"statusLine\" already in ~/.claude/settings.json."
        ),
        "{note}"
    );
    assert!(
        note.contains(
            r#"set "statusLine": {"type":"command","command":"command -v by >/dev/null 2>&1 && by statusline --harness claude-code || true"}"#
        ),
        "{note}"
    );
    // The summary does not claim a status line it did not install.
    assert_eq!(
        plan.summary[0],
        format!(
            "Claude Code: {} skills, 3 commands, 2 hooks in ~/.claude/settings.json",
            branchyard_setup::skills::SKILLS.len()
        )
    );

    // Theirs is the very line Branchyard would set: nothing to say.
    let ours = r#"{"statusLine": {"type": "command", "command": "command -v by >/dev/null 2>&1 && by statusline --harness claude-code || true"}}"#;
    let (plan, notes) = claude_plan(&[(".claude/settings.json", ours)]);
    assert!(plan.valid);
    assert!(!notes.iter().any(|n| n.contains("kept")), "{notes:?}");
    // An unparseable file is not "set": the merge reports it as it always did.
    let (plan, _) = claude_plan(&[(".claude/settings.json", "{ // comment\n}\n")]);
    assert!(!plan.valid);
}

#[test]
fn unless_set_is_for_a_top_level_json_merge_only() {
    let bad = Bundle(&[
        (
            "install/x.toml",
            "harness = \"x\"\nbinaries = [\"x\"]\n[[file]]\nfrom = \"a.toml\"\nto = \"~/a.toml\"\nhow = \"merge-toml\"\nunless_set = \"k\"\n[[file]]\nfrom = \"b.json\"\nto = \"~/b.json\"\nhow = \"merge-json\"\nat = \"ui\"\nunless_set = \"k\"\n",
        ),
        ("a.toml", "k = 1\n"),
        ("b.json", "{\"k\": 1}"),
    ]);
    let e = bad.manifests().unwrap_err();
    assert_eq!(
        e.matches("`unless_set` is only for merge-json at the top level")
            .count(),
        2,
        "{e}"
    );
}

#[test]
fn summary_counts_distinct_skill_roots_without_treating_payloads_as_hook_places() {
    let bundle = Bundle(&[
        ("skills/INDEX.md", "topic discovery"),
        ("skills/topics/design/INDEX.md", "design skill discovery"),
        ("skills/pptx/SKILL.md", "presentation skill"),
        ("skills/pptx/INDEX.md", "presentation resource discovery"),
        ("skills/pptx/scripts/run.py", "print('pptx')"),
        ("skills/artifact-tools/SKILL.md", "toolkit skill"),
        ("skills/artifact-tools/runtime.lock.json", "{}"),
        ("skills/loose.py", "print('not a skill')"),
        (
            "harnesses/fixture/hooks.json",
            r#"{"hooks":{"SessionStart":[]}}"#,
        ),
    ]);
    let manifest = toml::from_str(
        r#"
harness = "fixture"
binaries = ["fixture"]
provides = ["skills", "hooks"]
[[file]]
from = "skills"
to = "~/.agents/skills"
how = "write"
[[file]]
from = "skills/pptx/SKILL.md"
to = "~/.agents/skills/pptx/SKILL.md"
how = "write"
[[file]]
from = "skills/pptx/scripts/run.py"
to = "~/.agents/skills/pptx/scripts/run.py"
how = "write"
[[file]]
from = "harnesses/fixture/hooks.json"
to = "~/.fixture/hooks.json"
how = "merge-json"
"#,
    )
    .unwrap();
    assert!(bundle.check(&manifest, "fixture").is_empty());
    assert_eq!(
        install::summary_from(bundle, &manifest, &[]),
        "2 skills, 1 hook in ~/.fixture/hooks.json"
    );
}

#[test]
fn every_built_harness_summarizes_what_it_gets_and_where() {
    let manifests = install::manifests().unwrap();
    let line =
        |id: &str| install::summary(manifests.iter().find(|m| m.harness == id).unwrap(), &[]);
    assert_eq!(
        line("claude-code"),
        format!(
            "{} skills, 3 commands, 2 hooks, status line in ~/.claude/settings.json",
            branchyard_setup::skills::SKILLS.len()
        )
    );
    assert_eq!(
        line("codex"),
        format!(
            "{} skills, 2 hooks in ~/.codex/hooks.json",
            branchyard_setup::skills::SKILLS.len()
        )
    );
    assert_eq!(
        line("gemini-cli"),
        "3 commands, 1 hook in ~/.gemini/settings.json"
    );
    assert_eq!(
        line("qwen-code"),
        "2 hooks, status line in ~/.qwen/settings.json"
    );
    assert_eq!(
        line("antigravity"),
        format!("{} skills, 2 hooks, status line in ~/.gemini/config/hooks.json and ~/.gemini/antigravity-cli/settings.json", branchyard_setup::skills::SKILLS.len())
    );
    assert_eq!(
        line("droid"),
        "2 hooks, status line in ~/.factory/hooks.json and ~/.factory/settings.json"
    );
    // Code and YAML are not counted, only named.
    assert_eq!(
        line("amp"),
        "hooks, status line in ~/.config/amp/plugins/branchyard.ts"
    );
    assert_eq!(line("hermes"), "hooks in ~/.hermes/config.yaml");
    for m in &manifests {
        let text = install::summary(m, &[]);
        assert!(
            !text.is_empty() && !text.contains("{project}"),
            "{}: {text}",
            m.harness
        );
    }
}

#[test]
fn a_plans_diffs_name_files_under_the_home_relative_to_it() {
    let (plan, _) = claude_plan(&[]);
    let skill = file(&plan, "/home/dev/.claude/skills/setup/SKILL.md");
    let diff = skill.diff.as_deref().unwrap();
    assert!(
        diff.starts_with("--- /dev/null\n+++ b/.claude/skills/setup/SKILL.md\n"),
        "{diff}"
    );
    assert!(!diff.contains("b//"), "{diff}");
    // The file's own path is the one written.
    assert_eq!(skill.path, "/home/dev/.claude/skills/setup/SKILL.md");

    // An existing file diffs from `a/` to `b/`, both relative.
    let (plan, _) = claude_plan(&[(".claude/settings.json", "{\"theme\": \"dark\"}\n")]);
    let diff = file(&plan, "/home/dev/.claude/settings.json")
        .diff
        .as_deref()
        .unwrap();
    assert!(
        diff.starts_with("--- a/.claude/settings.json\n+++ b/.claude/settings.json\n"),
        "{diff}"
    );
}

#[test]
fn the_plugin_topic_has_one_installer_and_one_question() {
    let mut probe = FakeProbe::typical();
    probe.tools.insert("claude".into(), "2.1.310".into());
    let go = |answers: BTreeMap<String, Value>, defaults: bool| {
        next(
            Topic::Plugin,
            &probe,
            &answers,
            defaults,
            &mut branchyard_setup::probe::CountingEntropy(0),
            &BuiltinValidator,
        )
    };
    // Which harnesses is the only thing asked, and the detected ones are chosen.
    let first = go(BTreeMap::new(), false);
    let ids: Vec<&str> = first.questions.iter().map(|q| q.id.as_str()).collect();
    assert_eq!(ids, ["harnesses"]);
    assert_eq!(first.questions[0].default, json!(["claude-code"]));
    assert!(first.plan.is_none());
    // --defaults is the same as naming the detected ones.
    let defaulted = go(BTreeMap::new(), true);
    let plan = defaulted.plan.unwrap();
    assert!(
        plan.summary[0].starts_with(&format!(
            "Claude Code: {} skills",
            branchyard_setup::skills::SKILLS.len()
        )),
        "{:?}",
        plan.summary
    );
    assert_eq!(plan.commands[0].command, "by setup");
    // The retired skills-only host answers are ignored, not obeyed.
    let old: BTreeMap<String, Value> = [
        ("host".to_owned(), json!("codex-user")),
        ("harnesses".to_owned(), json!(["codex"])),
    ]
    .into();
    let old = go(old, false);
    assert_eq!(old.unknown_answers, ["host"]);
    let plan = old.plan.unwrap();
    assert!(
        plan.files
            .iter()
            .all(|f| !f.path.contains("/.codex/skills")),
        "{:#?}",
        plan.files
    );
    assert_eq!(
        plan.summary,
        [format!(
            "Codex: {} skills, 2 hooks in ~/.codex/hooks.json",
            branchyard_setup::skills::SKILLS.len()
        )]
    );
    assert!(plan
        .files
        .iter()
        .any(|f| f.path == "/home/dev/.agents/skills/pptx/SKILL.md"));
    assert!(plan
        .files
        .iter()
        .any(|f| f.path == "/home/dev/.agents/skills/artifact-tools/scripts/run.py"));
    // Choosing none installs nothing and points at the listing.
    let none = go([("harnesses".to_owned(), json!([]))].into(), false);
    let plan = none.plan.unwrap();
    assert!(plan.files.is_empty());
    assert_eq!(plan.summary, ["No harness chosen; nothing to install"]);
    assert_eq!(plan.commands[0].command, "by init plugin");
}
