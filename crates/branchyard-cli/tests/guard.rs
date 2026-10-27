//! The file guard through the built `by` (docs/modes.md#the-file-guard):
//! `--permissions edit-worktree` and `read-only` said before the run, held
//! below the fake agent (whose `WRITE` and `SH` ask no permission, as a
//! harness that routes nothing to Branchyard), and recorded on the branch
//! for `by show`, `by inspect` and `by ls --json`; `--yes` unguarded; a
//! delegating harness whose `by spawn` reaches the engine from inside the
//! guard, and a child guarded in its own worktree; a host without the
//! guard (`BRANCHYARD_GUARD=off`) saying so in one note and running
//! unguarded; `by doctor`'s `host.landlock` row. Hermetic: the fake agent,
//! git and `sh`.
//!
//! The guarded cases skip, printing why, on a host without Landlock.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
use std::process::Output;

use serde_json::{json, Value};

fn repo() -> branchyard_testkit::Repo {
    branchyard_testkit::repo!()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Whether this host can hold a harness's writes; says why not when not.
fn guarded(test: &str) -> bool {
    match branchyard::guard::support() {
        Ok(_) => true,
        Err(why) => {
            eprintln!("SKIPPED {test}: no file guard on this host: {why}");
            false
        }
    }
}

/// The harness's reply on `branch`'s turns, joined.
fn reply(repo: &branchyard_testkit::Repo, branch: &str) -> String {
    repo.json(&["log", branch, "--json"])
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["event"]["type"] == "message_delta")
        .map(|e| e["event"]["text"].as_str().unwrap().to_owned())
        .collect()
}

/// The `by: guard:` or `by: note:` line a run printed about the guard.
fn guard_line(out: &Output) -> Option<String> {
    stderr(out)
        .lines()
        .find(|l| l.starts_with("by: guard:") || l.contains("no file guard"))
        .map(str::to_owned)
}

#[test]
fn edit_worktree_and_read_only_are_held_said_and_recorded() {
    if !guarded("edit_worktree_and_read_only_are_held_said_and_recorded") {
        return;
    }
    let repo = repo();
    let edits = repo.by_agent(&[
        "run",
        "WRITE inside.txt=in WRITE ../outside.txt=out",
        "--name",
        "edits",
        "--permissions",
        "edit-worktree",
    ]);
    assert!(edits.status.success(), "{}", stderr(&edits));
    assert_eq!(
        guard_line(&edits).as_deref(),
        Some(
            "by: guard: landlock (edit-worktree): the harness can write only in its worktree, \
             its git directory, your own HOME, its temporary directory and /dev"
        )
    );
    let said = reply(&repo, "edits");
    assert!(said.contains("wrote inside.txt"), "{said}");
    assert!(
        said.contains("write ../outside.txt failed: Permission denied"),
        "{said}"
    );
    assert!(!repo.root.join(".branchyard/worktrees/outside.txt").exists());

    let shown = repo.json(&["show", "edits", "--json"]);
    assert_eq!(shown["status"]["state"], "ready");
    assert_eq!(shown["guard"]["kind"], "landlock");
    assert_eq!(shown["guard"]["preset"], "edit-worktree");
    let writable: Vec<&str> = shown["guard"]["writable"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    assert_eq!(writable[0], shown["worktree"].as_str().unwrap());
    assert!(writable.contains(&"/dev"), "{writable:?}");
    let text = stdout(&repo.ok(&["show", "edits"]));
    assert!(
        text.lines().any(|l| l.starts_with("guard")
            && l.contains("landlock (edit-worktree, ABI ")
            && l.ends_with("writes only in its worktree, its git directory, HOME, its temporary directory and /dev")),
        "{text}"
    );
    assert_eq!(
        repo.json(&["inspect", "edits", "--json"])["guard"],
        shown["guard"]
    );
    let list = repo.json(&["ls", "--json"]);
    assert_eq!(list[0]["guard"], shown["guard"]);

    let reads = repo.by_agent(&[
        "run",
        "WRITE notes.txt=x",
        "--name",
        "reads",
        "--permissions",
        "read-only",
        "--isolated",
    ]);
    assert!(reads.status.success(), "{}", stderr(&reads));
    let line = guard_line(&reads).unwrap();
    assert!(
        line.starts_with("by: guard: landlock (read-only):"),
        "{line}"
    );
    assert!(line.contains("its private HOME"), "{line}");
    assert!(line.ends_with("not its worktree"), "{line}");
    let said = reply(&repo, "reads");
    assert!(
        said.contains("write notes.txt failed: Permission denied"),
        "{said}"
    );
    let shown = repo.json(&["show", "reads", "--json"]);
    assert_eq!(shown["status"]["state"], "no_changes");
    assert_eq!(shown["guard"]["preset"], "read-only");
    let home = repo.root.join(".branchyard/homes/reads");
    assert_eq!(shown["guard"]["writable"][0], home.display().to_string());
}

#[test]
fn yes_runs_unguarded_and_the_record_says_why() {
    let repo = repo();
    let out = repo.by_agent(&["run", "WRITE ../free.txt=out", "--name", "free", "--yes"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(guard_line(&out), None, "{}", stderr(&out));
    assert!(repo.root.join(".branchyard/worktrees/free.txt").exists());
    assert_eq!(
        repo.json(&["show", "free", "--json"])["guard"],
        json!({"kind": "none", "reason": "no permission preset names a file scope (--yes, --ask \
                                         and explicit rules name none)"})
    );
    let text = stdout(&repo.ok(&["show", "free"]));
    assert!(
        text.lines()
            .any(|l| l.starts_with("guard") && l.contains("none: no permission preset")),
        "{text}"
    );
}

#[test]
fn a_host_without_the_guard_says_so_once_and_runs_unguarded() {
    let mut repo = repo();
    repo.set_env("BRANCHYARD_GUARD", "off");
    let out = repo.by_agent(&[
        "run",
        "WRITE ../loose.txt=out",
        "--name",
        "loose",
        "--permissions",
        "edit-worktree",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stderr(&out);
    let notes: Vec<&str> = text.lines().filter(|l| l.contains("file guard")).collect();
    assert_eq!(
        notes,
        [
            "by: note: no file guard on this host (BRANCHYARD_GUARD=off turns it off): \
          --permissions edit-worktree decides only the tool calls the harness asks Branchyard \
          about"
        ],
        "{text}"
    );
    assert!(repo.root.join(".branchyard/worktrees/loose.txt").exists());
    assert_eq!(
        repo.json(&["show", "loose", "--json"])["guard"],
        json!({"kind": "none", "reason": "this host cannot hold it: BRANCHYARD_GUARD=off turns it off"})
    );

    let doctor: Value = serde_json::from_slice(&repo.by(&["doctor", "--json"]).stdout).unwrap();
    let row = doctor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "host.landlock")
        .unwrap();
    assert_eq!(row["status"], "warn");
    assert_eq!(
        row["detail"],
        "no file guard: BRANCHYARD_GUARD=off turns it off"
    );
    assert!(row["fix"]
        .as_str()
        .unwrap()
        .contains("docs/modes.md#the-file-guard"));
}

#[test]
fn doctor_names_the_landlock_abi_it_found() {
    let Ok(abi) = branchyard::guard::support() else {
        eprintln!("SKIPPED doctor_names_the_landlock_abi_it_found: no file guard on this host");
        return;
    };
    let repo = repo();
    repo.ok(&["ls"]);
    let doctor: Value = serde_json::from_slice(&repo.by(&["doctor", "--json"]).stdout).unwrap();
    let row = doctor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "host.landlock")
        .unwrap();
    assert_eq!(row["status"], "ok");
    assert!(
        row["detail"]
            .as_str()
            .unwrap()
            .starts_with(&format!("Landlock ABI {abi}: ")),
        "{row}"
    );
}

/// The parsed JSON a `SH by ... --json` line printed, after its `sh:` line.
fn sh_json(reply: &str, n: usize) -> (i32, Value) {
    let part = reply
        .split("sh: ")
        .nth(1 + n)
        .unwrap_or_else(|| panic!("no command {n} in {reply}"));
    let (status, rest) = part.split_once('\n').unwrap();
    let value = serde_json::Deserializer::from_str(rest)
        .into_iter::<Value>()
        .next()
        .and_then(Result::ok)
        .unwrap_or(Value::Null);
    (status.parse().unwrap(), value)
}

#[test]
fn a_guarded_harness_delegates_and_its_child_is_guarded_in_its_own_worktree() {
    if !guarded("a_guarded_harness_delegates_and_its_child_is_guarded_in_its_own_worktree") {
        return;
    }
    let repo = repo();
    let prompt = [
        "SH by spawn 'WRITE kid.txt=k WRITE ../kid-outside.txt=o' --name kid --wait --json",
        "SH by inspect --json",
        "SH printf r > root.txt && echo root-inside-ok",
        "SH printf o > ../root-outside.txt || echo root-outside-refused",
    ]
    .join("\n");
    let out = repo.by_agent(&[
        "run",
        &prompt,
        "--name",
        "root",
        "--delegate",
        "--permissions",
        "edit-worktree",
    ]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    let said = reply(&repo, "root");
    // `by spawn` reached the engine's broker from inside the guard.
    let (code, kid) = sh_json(&said, 0);
    assert_eq!((code, kid["name"].as_str()), (0, Some("kid")), "{said}");
    assert_eq!(kid["status"]["state"], "ready", "{said}");
    let (code, me) = sh_json(&said, 1);
    assert_eq!((code, me["name"].as_str()), (0, Some("root")), "{said}");
    assert!(said.contains("root-inside-ok"), "{said}");
    assert!(said.contains("root-outside-refused"), "{said}");
    assert!(said.contains("Permission denied"), "{said}");
    let worktrees = repo.root.join(".branchyard/worktrees");
    assert!(!worktrees.join("root-outside.txt").exists());

    // The child ran on the engine, outside its parent's restriction, under
    // its own guard: its worktree writable, beside it refused.
    let kid_said = reply(&repo, "kid");
    assert!(kid_said.contains("wrote kid.txt"), "{kid_said}");
    assert!(
        kid_said.contains("write ../kid-outside.txt failed: Permission denied"),
        "{kid_said}"
    );
    assert!(!worktrees.join("kid-outside.txt").exists());
    let shown = repo.json(&["show", "kid", "--json"]);
    assert_eq!(shown["guard"]["kind"], "landlock");
    assert_eq!(shown["guard"]["preset"], "edit-worktree");
    assert_eq!(shown["guard"]["writable"][0], shown["worktree"]);
    assert_eq!(
        repo.json(&["show", "root", "--json"])["guard"]["kind"],
        "landlock"
    );
}
