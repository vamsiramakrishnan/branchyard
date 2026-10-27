//! A golden test per harness whose login `catalog/logins.toml` describes
//! (docs/adding-a-harness.md): for each case in `golden/logins/ID.toml`,
//! the detection script runs with `/bin/sh` against a fake home, a fake
//! environment and fake executables, and must print exactly the case's
//! `lines` (its `env`, `file` and `fact` lines, by name only) and make
//! exactly its LOGIN cell, state, evidence and detail. Nothing real runs:
//! the harness is a shell script that prints a version and the case's status
//! output, `PATH` holds only it, the case's other fakes and the few host
//! programs the script uses, and every key value is made up. Requires `sh`.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use branchyard::inventory::{self, DetectOptions};
use branchyard_controls::catalog;
use branchyard_recipe::quote;
use serde::Deserialize;

/// The host programs the detection script runs, linked into the case's
/// `PATH`: nothing else of the host's is on it, so a case decides whether
/// `sqlite3`, `python3` or `bun` exist.
const HOST_TOOLS: &[&str] = &[
    "cat", "cut", "grep", "head", "hostname", "mkdir", "mktemp", "rm", "sed", "sleep", "tr",
    "uname",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Golden {
    #[serde(rename = "case")]
    cases: Vec<Case>,
}

/// One machine state and what detection must make of it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    /// Variables set for the script (made-up values; `{home}` is the fake
    /// home's path).
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// Files under the fake home, by path relative to it.
    #[serde(default)]
    home: BTreeMap<String, String>,
    /// Other fake programs on `PATH` (`sqlite3`, `bun`, `uname`), as `sh`
    /// bodies.
    #[serde(default)]
    programs: BTreeMap<String, String>,
    /// The harness executable's first line; `#!/bin/sh` unless the case
    /// needs it to be a script for another interpreter.
    #[serde(default)]
    first_line: Option<String>,
    /// What its status command prints, and its exit status.
    #[serde(default)]
    status: Vec<String>,
    #[serde(default)]
    status_exit: i32,
    /// The `env`, `file` and `fact` lines detection prints for the
    /// harness, without the harness ID.
    lines: Vec<String>,
    /// The LOGIN cell (`Login::label`).
    cell: String,
    state: String,
    evidence: String,
    detail: String,
    #[serde(default)]
    key: Option<String>,
    /// The version note, when the case is about one.
    #[serde(default)]
    version_note: Option<String>,
}

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/logins")
}

fn write_exec(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Where the host keeps `program`, from the usual directories.
fn host(program: &str) -> Option<PathBuf> {
    ["/usr/bin", "/bin", "/usr/local/bin"]
        .iter()
        .map(|dir| Path::new(dir).join(program))
        .find(|path| path.is_file())
}

/// What detection makes of `case` for harness `id`: the lines it printed
/// for the harness, and the harness's state.
fn run(id: &str, case: &Case) -> (Vec<String>, inventory::HarnessState) {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let (bin, tools, home) = (base.join("bin"), base.join("tools"), base.join("home"));
    for d in [&bin, &tools, &home] {
        fs::create_dir_all(d).unwrap();
    }
    for tool in HOST_TOOLS {
        if let Some(path) = host(tool) {
            std::os::unix::fs::symlink(path, tools.join(tool)).unwrap();
        }
    }
    let program = &catalog::harness(id).unwrap().binaries[0];
    let status: Vec<String> = case.status.iter().map(|l| quote(l)).collect();
    write_exec(
        &bin.join(program),
        &format!(
            "{}\ncase \"$1\" in --version|version) echo 1.0.0; exit 0;; esac\n{}exit {}\n",
            case.first_line.as_deref().unwrap_or("#!/bin/sh"),
            match status.is_empty() {
                true => String::new(),
                false => format!("printf '%s\\n' {}\n", status.join(" ")),
            },
            case.status_exit
        ),
    );
    for (name, body) in &case.programs {
        write_exec(&bin.join(name), &format!("#!/bin/sh\n{body}\n"));
    }
    for (relative, text) in &case.home {
        let path = home.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    let script = inventory::script(&DetectOptions {
        timeout: Duration::from_secs(10),
        dirs: vec![],
        only: Some(vec![id.to_owned()]),
    });
    let mut child = Command::new("/bin/sh")
        .arg("-s")
        .env_clear()
        .env("PATH", format!("{}:{}", bin.display(), tools.display()))
        .env("HOME", &home)
        .envs(
            case.env
                .iter()
                .map(|(name, value)| (name, value.replace("{home}", &home.display().to_string()))),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), script.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let lines = text
        .lines()
        .filter_map(|line| {
            let (tag, rest) = line.split_once(' ')?;
            let rest = rest.strip_prefix(id)?.strip_prefix(' ')?;
            matches!(tag, "env" | "file" | "fact").then(|| format!("{tag} {rest}"))
        })
        .collect();
    let found = inventory::parse(&text, 0).unwrap_or_else(|e| {
        panic!(
            "{id}: {e}\n{text}\n{}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    let state = found
        .get(id)
        .unwrap_or_else(|| panic!("{id} was not found:\n{text}"))
        .clone();
    (lines, state)
}

fn word<T: serde::Serialize>(value: T) -> String {
    serde_json::to_value(value)
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn every_login_table_has_a_golden_and_every_golden_names_a_harness() {
    let goldens: Vec<String> = fs::read_dir(golden_dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .map(|p| p.file_stem().unwrap().to_string_lossy().into_owned())
        .collect();
    for id in catalog::logins().keys() {
        assert!(
            goldens.contains(id),
            "catalog/logins.toml describes {id}, and crates/branchyard/tests/golden/logins/{id}.toml \
             is missing (docs/adding-a-harness.md)"
        );
    }
    for id in &goldens {
        let harness =
            catalog::harness(id).unwrap_or_else(|| panic!("{id}.toml: not in the catalog"));
        assert!(
            !harness.binaries.is_empty(),
            "{id}: detection never finds it"
        );
    }
}

#[test]
fn each_harness_login_reads_as_its_golden_cases_say() {
    let mut failures = Vec::new();
    let mut cases = 0;
    let mut paths: Vec<PathBuf> = fs::read_dir(golden_dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    paths.sort();
    for path in paths {
        let id = path.file_stem().unwrap().to_string_lossy().into_owned();
        let golden: Golden = toml_edit::de::from_str(&fs::read_to_string(&path).unwrap())
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert!(!golden.cases.is_empty(), "{id}: no case");
        for case in &golden.cases {
            cases += 1;
            let (lines, state) = run(&id, case);
            let login = &state.login;
            let got = [
                ("lines", format!("{lines:?}"), format!("{:?}", case.lines)),
                ("cell", login.label(), case.cell.clone()),
                ("state", word(login.state), case.state.clone()),
                ("evidence", word(login.evidence), case.evidence.clone()),
                ("detail", login.detail.clone(), case.detail.clone()),
                ("key", format!("{:?}", login.key), format!("{:?}", case.key)),
            ];
            for (what, actual, want) in got {
                if actual != want {
                    failures.push(format!(
                        "{id} / {}: {what}\n  want: {want}\n  got:  {actual}",
                        case.name
                    ));
                }
            }
            if let Some(want) = &case.version_note {
                if state.version_note.as_ref() != Some(want) {
                    failures.push(format!(
                        "{id} / {}: version_note\n  want: {want}\n  got:  {:?}",
                        case.name, state.version_note
                    ));
                }
            }
            // A made-up key value never reaches the inventory.
            let json = serde_json::to_string(&state).unwrap();
            for value in case.env.values().filter(|v| v.starts_with("fake-")) {
                assert!(!json.contains(value.as_str()), "{id}: a value was printed");
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {cases} cases differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
