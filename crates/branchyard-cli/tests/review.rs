//! `by review`, end to end with the built `by`, the fake ACP agent and a
//! fake editor: a shell script that adds comment lines to the review file
//! the way a person would. Hermetic: no editor, harness or network is used.
//! Requires `git`, `sh` and `sed`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Output;

use branchyard_testkit::fake_agent;
use branchyard_testkit::wait;
use serde_json::Value;

/// The kit's repository, plus what this file adds.
struct Repo(branchyard_testkit::Repo);

impl std::ops::Deref for Repo {
    type Target = branchyard_testkit::Repo;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Repo {
    fn new() -> Repo {
        let mut kit = branchyard_testkit::repo!(&[("a.txt", "one\nkeep\n")]);
        kit.remove_env("VISUAL");
        kit.remove_env("EDITOR");
        Repo(kit)
    }

    /// A fake editor: runs `script` with the review file as `$1`.
    fn editor(&self, name: &str, script: &str) -> String {
        let path = self.dir.join(name);
        fs::write(&path, format!("#!/bin/sh\nset -e\n{script}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path.display().to_string()
    }

    /// `by <args>` with `EDITOR` set to `editor`.
    fn by(&self, editor: Option<&str>, args: &[&str]) -> Output {
        let mut command = self.command(env!("CARGO_BIN_EXE_by"));
        if let Some(editor) = editor {
            command.env("EDITOR", editor);
        }
        command.args(args).output().unwrap()
    }

    fn agent(&self) -> String {
        fake_agent!().display().to_string()
    }

    fn ok(&self, out: Output) -> String {
        assert!(
            out.status.success(),
            "stdout:\n{}\nstderr:\n{}",
            stdout(&out),
            stderr(&out)
        );
        stdout(&out)
    }

    fn prompts(&self, branch: &str) -> Vec<String> {
        let out = self.ok(self.by(None, &["log", branch, "--json"]));
        let events: Value = serde_json::from_str(&out).unwrap();
        events
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["activity"] == "prompt")
            .map(|e| e["text"].as_str().unwrap().to_owned())
            .collect()
    }

    fn draft(&self, branch: &str) -> PathBuf {
        self.root
            .join(".branchyard/review")
            .join(format!("{branch}.diff"))
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The fake editor's comments: one on the added line, a two-line one on
/// the hunk (the fake agent rewrote the file, so it is one line), one on
/// the whole file.
const COMMENT: &str = r#"sed -i \
  -e '/^+two$/a >> please WRITE review.txt=done now' \
  -e '/^@@ /a >> Is this hunk tested?\n>> It changes behaviour.' \
  -e '/^+++ b\/a.txt$/a >> Rename the file' "$1""#;

const EXPECTED: &str = "File: a.txt\nScope: file\nUser comment: \"Rename the file\"\n\n\
     File: a.txt\nLine: 1\nUser comment: \"Is this hunk tested?\\nIt changes behaviour.\"\n\n\
     File: a.txt\nLine: 1\nUser comment: \"please WRITE review.txt=done now\"";

#[test]
fn comments_written_on_the_diff_go_to_the_branch_as_one_prompt() {
    let repo = Repo::new();
    let agent = repo.agent();
    repo.ok(repo.by(
        None,
        &[
            "run",
            "WRITE a.txt=two",
            "--name",
            "feat",
            "--harness",
            "gemini-cli",
            "--command",
            &agent,
        ],
    ));
    let commenting = repo.editor("comment.sh", COMMENT);

    // --print shows the prompt and sends nothing; the comments are kept.
    let printed = repo.ok(repo.by(Some(&commenting), &["review", "feat", "--print"]));
    assert!(
        printed.starts_with(
            "Review comments on this branch's changes (3 comments on 1 file), written on its \
             diff with by review."
        ),
        "{printed}"
    );
    assert!(printed.contains(EXPECTED), "{printed}");
    assert_eq!(repo.prompts("feat").len(), 1);
    assert!(repo.draft("feat").is_file());

    // The next review reopens the draft: an editor that changes nothing
    // sends the same comments.
    let out = repo.by(
        Some("true"),
        &["review", "feat", "--yes", "--command", &agent],
    );
    let text = repo.ok(out);
    assert!(text.contains("wrote review.txt"), "{text}");
    let prompts = repo.prompts("feat");
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[1], printed);
    assert!(
        !repo.draft("feat").exists(),
        "a sent review leaves no draft"
    );

    // No comments: nothing is sent.
    let out = repo.by(
        Some("true"),
        &["review", "feat", "--yes", "--command", &agent],
    );
    assert!(stderr(&out).contains("no comments"), "{}", stderr(&out));
    assert!(out.status.success());
    assert_eq!(repo.prompts("feat").len(), 2);
}

#[test]
fn an_edited_diff_is_refused_and_kept_and_a_file_can_be_given() {
    let repo = Repo::new();
    let agent = repo.agent();
    repo.ok(repo.by(
        None,
        &[
            "run",
            "WRITE a.txt=two",
            "--name",
            "feat",
            "--harness",
            "gemini-cli",
            "--command",
            &agent,
        ],
    ));
    let breaking = repo.editor(
        "break.sh",
        r#"sed -i -e 's/^+two$/+three/' -e '/^+three$/a >> hm' "$1""#,
    );
    let out = repo.by(Some(&breaking), &["review", "feat", "--print"]);
    assert_eq!(out.status.code(), Some(1));
    let error = stderr(&out);
    assert!(error.contains("the diff was edited here"), "{error}");
    assert!(error.contains("nothing was sent"), "{error}");
    assert!(repo.draft("feat").is_file(), "the review is kept to fix");

    // --file reads an edited review without an editor.
    let file = repo.dir.join("mine.diff");
    let draft = fs::read_to_string(repo.draft("feat")).unwrap();
    fs::write(&file, draft.replace("+three\n>> hm\n", "+two\n>> fine\n")).unwrap();
    let printed = repo.ok(repo.by(
        None,
        &[
            "review",
            "feat",
            "--print",
            "--file",
            file.to_str().unwrap(),
        ],
    ));
    assert!(
        printed.contains("File: a.txt\nLine: 1\nUser comment: \"fine\""),
        "{printed}"
    );

    // No editor at all is explained.
    let out = repo.by(None, &["review", "feat"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("no editor"), "{}", stderr(&out));
    // A branch with no changes has nothing to review.
    repo.ok(repo.by(
        None,
        &[
            "run",
            "say hello",
            "--name",
            "idle",
            "--harness",
            "gemini-cli",
            "--command",
            &agent,
        ],
    ));
    let out = repo.by(Some("true"), &["review", "idle"]);
    assert!(
        stderr(&out).contains("has no changes to review"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn detach_sends_in_the_background() {
    let repo = Repo::new();
    let agent = repo.agent();
    repo.ok(repo.by(
        None,
        &[
            "run",
            "WRITE a.txt=two",
            "--name",
            "feat",
            "--harness",
            "gemini-cli",
            "--command",
            &agent,
        ],
    ));
    let commenting = repo.editor("comment.sh", COMMENT);
    let out = repo.by(
        Some(&commenting),
        &["review", "feat", "--detach", "--yes", "--command", &agent],
    );
    let error = stderr(&out);
    assert!(out.status.success(), "{error}");
    assert!(error.contains("sending in the background"), "{error}");
    wait::until("the background send to run", || {
        repo.prompts("feat").len() >= 2
    });
    assert!(repo.prompts("feat")[1].contains(EXPECTED));
    // Let the background turn end before the repository is removed.
    wait::until("the background turn to end", || {
        let show: Value =
            serde_json::from_str(&repo.ok(repo.by(None, &["show", "feat", "--json"]))).unwrap();
        show["status"] != "running"
    });
    // Other send options cannot be carried to the background.
    let out = repo.by(
        Some(&commenting),
        &["review", "feat", "--detach", "--max-turns", "2"],
    );
    assert!(
        stderr(&out).contains("--detach carries only"),
        "{}",
        stderr(&out)
    );
}
