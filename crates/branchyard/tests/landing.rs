//! Landing through the SDK against the fake ACP agent: a child its parent
//! left to the engine is integrated into the parent's branch once it is
//! ready while the parent waits on its children; one that cannot land is
//! sent a turn to fix its work, and left for its parent after
//! `LAND_FIXES` of them; and the parent's wake says what landed.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::fs;
use std::path::{Path, PathBuf};

use branchyard::{
    Activity, Branch, BranchStatus, Envelope, Policy, ReviewSpec, Spawn, TaskOptions,
};
use branchyard_testkit::wait;
use common::{fake_agent, Fixture};

/// Options that delegate, with `land` as the fake agent's script for a
/// turn the engine sends a child to make its work land.
fn delegating(f: &Fixture, land: Option<&Path>) -> TaskOptions {
    let agent = fake_agent().display().to_string();
    let command = match land {
        Some(script) => vec![
            "env".to_owned(),
            format!("FAKE_ACP_LAND={}", script.display()),
            agent,
        ],
        None => vec![agent],
    };
    TaskOptions {
        delegation: Some(Envelope::default()),
        delegation_server: Some(vec![fake_agent().display().to_string()]),
        policy: Policy::allow_all(),
        command: Some(command),
        ..f.options()
    }
}

/// A child's prompt that waits for `go` to exist, then runs `then` in its
/// worktree; so its parent's turn ends while it still runs.
fn held(go: &Path, then: &str) -> String {
    format!(
        "SH until [ -f {} ]; do sleep 0.05; done; {then}",
        go.display()
    )
}

fn landing(prompt: String, name: &str) -> Spawn {
    Spawn {
        prompt,
        name: Some(name.into()),
        land: true,
        ..Spawn::default()
    }
}

/// `root` after a first turn, its delegate, and the gate its held children
/// wait for.
struct Parent {
    root: Branch,
    delegate: branchyard::Delegate,
    options: TaskOptions,
    go: PathBuf,
}

fn parent(f: &Fixture, options: TaskOptions) -> Parent {
    let root = f
        .yard
        .task("say hi")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let delegate = root.delegate(options.clone()).unwrap();
    Parent {
        root,
        delegate,
        options,
        go: f.dir.join("go"),
    }
}

impl Parent {
    /// A turn of the parent that ends while its children run: it parks.
    fn end_turn(&self, prompt: &str) {
        let after = self.root.send(prompt, self.options.clone()).unwrap();
        assert_eq!(after.info().status, BranchStatus::WaitingOnChildren);
    }

    fn release(&self) {
        fs::write(&self.go, "").unwrap();
    }
}

fn status(f: &Fixture, name: &str) -> BranchStatus {
    f.yard.branch(name).unwrap().info().status.clone()
}

fn settled(f: &Fixture, name: &str) -> BranchStatus {
    wait::until(&format!("{name} to settle"), || {
        let status = status(f, name);
        (!matches!(
            status,
            BranchStatus::Running | BranchStatus::Waiting | BranchStatus::WaitingOnChildren
        ))
        .then_some(status)
    })
}

/// Its log's landing records, as `(branch, outcome)`.
fn landings(f: &Fixture, name: &str) -> Vec<(String, String)> {
    f.yard
        .branch(name)
        .unwrap()
        .events()
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.activity {
            Activity::Delegation {
                tool,
                branch,
                outcome,
                ..
            } if tool == "land" => Some((branch, outcome)),
            _ => None,
        })
        .collect()
}

/// The prompts of its turns, oldest first.
fn prompts(f: &Fixture, name: &str) -> Vec<String> {
    f.yard
        .branch(name)
        .unwrap()
        .events()
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.activity {
            Activity::Prompt(text) => Some(text),
            _ => None,
        })
        .collect()
}

fn merged_into_root(status: &BranchStatus) -> bool {
    matches!(status, BranchStatus::Merged { target, .. } if target == "by/root")
}

#[test]
fn a_ready_child_lands_while_its_parent_waits_and_its_wake_says_so() {
    let f = Fixture::new();
    let p = parent(&f, delegating(&f, None));
    let spawned = p
        .delegate
        .spawn(landing(held(&p.go, "echo k > kid.txt"), "kid"))
        .unwrap();
    assert!(spawned.land);
    assert!(p.delegate.inspect("kid").unwrap().land);
    p.end_turn("WRITE root.txt=r");
    p.release();

    assert_eq!(settled(&f, "root"), BranchStatus::Ready);
    assert!(
        merged_into_root(&status(&f, "kid")),
        "{:?}",
        status(&f, "kid")
    );
    // Its work and the parent's own are on the parent's branch, and in the
    // candidate its wake ended with.
    assert_eq!(f.git(&["show", "by/root:kid.txt"]), "k\n");
    assert_eq!(f.git(&["show", "by/root:root.txt"]), "r\n");
    let head = f.git(&["rev-parse", "by/root"]);
    let root = f.yard.branch("root").unwrap().info().clone();
    assert_eq!(root.candidate.unwrap().commit, head.trim());
    assert_eq!(root.turns, 3, "its own two, and the wake");

    let landed = landings(&f, "root");
    assert_eq!(landed.len(), 1, "{landed:?}");
    assert_eq!(landed[0].0, "kid");
    assert!(
        landed[0].1.starts_with("landed in by/root at "),
        "{landed:?}"
    );
    let wake = prompts(&f, "root").pop().unwrap();
    assert!(wake.contains("<branchyard-wake>"), "{wake}");
    assert!(
        wake.contains("Landed for you, so they are in your branch and its candidate now: kid."),
        "{wake}"
    );
    let inspected = p.delegate.inspect("kid").unwrap();
    let note = inspected.landing.unwrap();
    assert_eq!(note.fixes, 0);
    assert!(!note.left);
    assert!(note.note.unwrap().starts_with("landed in by/root"));
}

/// Delegated from outside a harness, its parent idle: the child lands as it
/// ends, and its parent, which had nothing, has its work without a turn.
#[test]
fn a_child_of_an_idle_parent_lands_without_waking_it() {
    let f = Fixture::new();
    let p = parent(&f, delegating(&f, None));
    assert_eq!(status(&f, "root"), BranchStatus::NoChanges);
    p.delegate
        .spawn(landing("WRITE kid.txt=k".into(), "kid"))
        .unwrap();
    p.root.wait_subtree().unwrap();

    assert!(
        merged_into_root(&status(&f, "kid")),
        "{:?}",
        status(&f, "kid")
    );
    assert_eq!(f.git(&["show", "by/root:kid.txt"]), "k\n");
    let root = f.yard.branch("root").unwrap().info().clone();
    assert_eq!(root.status, BranchStatus::Ready, "it has work now");
    assert_eq!(root.turns, 1, "not woken");
    let head = f.git(&["rev-parse", "by/root"]);
    assert_eq!(root.candidate.unwrap().commit, head.trim());
    let log = f.yard.branch("root").unwrap().events().unwrap();
    assert!(log
        .iter()
        .any(|e| e.activity == Activity::Status(BranchStatus::Ready)));
    assert_eq!(landings(&f, "root").len(), 1);
}

#[test]
fn a_child_whose_check_fails_on_the_merge_is_sent_a_turn_to_fix_it_then_lands() {
    let f = Fixture::new();
    let script = f.dir.join("land.txt");
    fs::write(&script, "SH echo fixed > kid.txt\n").unwrap();
    let p = parent(&f, delegating(&f, Some(&script)));
    p.delegate
        .spawn(Spawn {
            check: Some(vec![
                "sh".into(),
                "-c".into(),
                "grep -q fixed kid.txt".into(),
            ]),
            ..landing(held(&p.go, "echo broken > kid.txt"), "kid")
        })
        .unwrap();
    p.end_turn("say hi again");
    p.release();

    assert_eq!(settled(&f, "root"), BranchStatus::Ready);
    assert!(
        merged_into_root(&status(&f, "kid")),
        "{:?}",
        status(&f, "kid")
    );
    assert_eq!(f.git(&["show", "by/root:kid.txt"]), "fixed\n");
    // The child was told what failed and how to make it land.
    let fix = prompts(&f, "kid").pop().unwrap();
    assert!(fix.contains("<branchyard-land>"), "{fix}");
    assert!(
        fix.contains("your check failed on its work merged into by/root"),
        "{fix}"
    );
    assert!(fix.contains("turn 1 of at most 2"), "{fix}");
    let outcomes: Vec<String> = landings(&f, "root").into_iter().map(|l| l.1).collect();
    assert_eq!(outcomes.len(), 2, "{outcomes:?}");
    assert!(
        outcomes[0].contains("its check failed")
            && outcomes[0].ends_with("sent it a turn to fix that (1 of 2)"),
        "{outcomes:?}"
    );
    assert!(outcomes[1].starts_with("landed in by/root"), "{outcomes:?}");
    assert_eq!(p.delegate.inspect("kid").unwrap().landing.unwrap().fixes, 1);
    // The parent is woken once, after its child landed.
    let root = f.yard.branch("root").unwrap().info().clone();
    assert_eq!(root.turns, 3);
}

#[test]
fn a_child_that_cannot_land_after_its_fixes_is_left_for_its_parent() {
    let f = Fixture::new();
    let script = f.dir.join("land.txt");
    fs::write(&script, "SH echo still broken > kid.txt\n").unwrap();
    let p = parent(&f, delegating(&f, Some(&script)));
    p.delegate
        .spawn(Spawn {
            check: Some(vec!["sh".into(), "-c".into(), "exit 1".into()]),
            ..landing(held(&p.go, "echo broken > kid.txt"), "kid")
        })
        .unwrap();
    p.end_turn("say hi again");
    p.release();

    assert_eq!(settled(&f, "root"), BranchStatus::NoChanges);
    assert_eq!(status(&f, "kid"), BranchStatus::Ready, "not merged");
    assert!(f
        .git(&["ls-tree", "--name-only", "by/root"])
        .lines()
        .all(|file| file != "kid.txt"));
    let note = p.delegate.inspect("kid").unwrap().landing.unwrap();
    assert_eq!(note.fixes, branchyard::LAND_FIXES);
    assert!(note.left);
    let why = note.note.unwrap();
    assert!(
        why.contains("its check failed")
            && why.ends_with("after 2 turns the engine sent it to fix that"),
        "{why}"
    );
    let fixes = prompts(&f, "kid")
        .iter()
        .filter(|p| p.contains("<branchyard-land>"))
        .count();
    assert_eq!(fixes, 2);
    let wake = prompts(&f, "root").pop().unwrap();
    assert!(
        wake.contains("Left for you, not landed: kid (its check failed"),
        "{wake}"
    );
}

#[test]
fn a_child_that_strays_outside_its_scope_undoes_it_and_lands() {
    let f = Fixture::new();
    let script = f.dir.join("land.txt");
    fs::write(&script, "SH git rm -q stray.txt\n").unwrap();
    let p = parent(&f, delegating(&f, Some(&script)));
    p.delegate
        .spawn(Spawn {
            owns: vec!["src/kid".into()],
            ..landing(
                held(
                    &p.go,
                    "mkdir -p src/kid && echo k > src/kid/a.txt && echo s > stray.txt",
                ),
                "kid",
            )
        })
        .unwrap();
    p.end_turn("say hi again");
    p.release();

    assert_eq!(settled(&f, "root"), BranchStatus::Ready);
    assert!(
        merged_into_root(&status(&f, "kid")),
        "{:?}",
        status(&f, "kid")
    );
    let files = f.git(&["ls-tree", "-r", "--name-only", "by/root"]);
    assert!(files.lines().any(|file| file == "src/kid/a.txt"), "{files}");
    assert!(!files.lines().any(|file| file == "stray.txt"), "{files}");
    let fix = prompts(&f, "kid").pop().unwrap();
    assert!(
        fix.contains("you changed stray.txt outside the paths you own (src/kid)"),
        "{fix}"
    );
}

#[test]
fn a_child_that_conflicts_merges_its_parents_branch_and_lands() {
    let f = Fixture::new();
    let script = f.dir.join("land.txt");
    fs::write(
        &script,
        "SH git merge -q by/root >/dev/null 2>&1; printf 'root\\nkid\\n' > shared.txt; \
         git add shared.txt; git commit -q -m 'merge by/root'\n",
    )
    .unwrap();
    let p = parent(&f, delegating(&f, Some(&script)));
    p.delegate
        .spawn(landing(held(&p.go, "echo kid > shared.txt"), "kid"))
        .unwrap();
    // The parent changes the same file meanwhile.
    p.end_turn("WRITE shared.txt=root");
    p.release();

    assert_eq!(settled(&f, "root"), BranchStatus::Ready);
    assert!(
        merged_into_root(&status(&f, "kid")),
        "{:?}",
        status(&f, "kid")
    );
    assert_eq!(f.git(&["show", "by/root:shared.txt"]), "root\nkid\n");
    let fix = prompts(&f, "kid").pop().unwrap();
    assert!(
        fix.contains("your work conflicts with your parent's branch by/root in shared.txt"),
        "{fix}"
    );
    assert!(fix.contains("`git merge by/root`"), "{fix}");
}

#[test]
fn siblings_sharing_their_parents_check_land_together_once_both_are_ready() {
    let f = Fixture::new();
    let options = TaskOptions {
        check: Some(vec![
            "sh".into(),
            "-c".into(),
            "test -f a.out && test -f b.out".into(),
        ]),
        ..delegating(&f, None)
    };
    let p = parent(&f, options);
    let go_b = f.dir.join("go-b");
    p.delegate
        .spawn(landing(held(&p.go, "echo a > a.out"), "a"))
        .unwrap();
    p.delegate
        .spawn(landing(held(&go_b, "echo b > b.out"), "b"))
        .unwrap();
    assert!(p.delegate.inspect("a").unwrap().check_inherited);
    p.end_turn("say hi again");

    // Alone, a would fail the check it shares: it waits for b.
    p.release();
    assert_eq!(settled(&f, "a"), BranchStatus::Ready);
    assert_eq!(status(&f, "root"), BranchStatus::WaitingOnChildren);
    assert!(landings(&f, "root").is_empty());
    fs::write(&go_b, "").unwrap();

    assert_eq!(settled(&f, "root"), BranchStatus::Ready);
    assert!(merged_into_root(&status(&f, "a")), "{:?}", status(&f, "a"));
    assert!(merged_into_root(&status(&f, "b")), "{:?}", status(&f, "b"));
    let landed: Vec<String> = landings(&f, "root").into_iter().map(|l| l.0).collect();
    assert_eq!(landed, ["a", "b"]);
}

/// A reviewer on the parent's own harness, answering from `rubric`: the
/// fake agent replies with the canned verdicts it names.
fn reviewed(prompt: String, name: &str, rubric: String) -> Spawn {
    Spawn {
        review: Some(ReviewSpec {
            harness: "gemini-cli".into(),
            rubric: Some(rubric),
            ..ReviewSpec::default()
        }),
        ..landing(prompt, name)
    }
}

/// Canned verdicts, answered in order.
fn verdicts(f: &Fixture, answers: &[&str]) -> PathBuf {
    let dir = f.dir.join("verdicts");
    fs::create_dir_all(&dir).unwrap();
    for (i, answer) in answers.iter().enumerate() {
        fs::write(dir.join(format!("{i}.json")), answer).unwrap();
    }
    dir
}

/// The reviewers spawned for `child`, as children of root.
fn reviewers(f: &Fixture, child: &str) -> Vec<String> {
    f.yard
        .branch("root")
        .unwrap()
        .info()
        .children
        .iter()
        .filter(|c| c.starts_with(&format!("review-{child}")))
        .cloned()
        .collect()
}

#[test]
fn a_reviewer_asks_for_changes_the_child_makes_them_and_it_lands() {
    let f = Fixture::new();
    let script = f.dir.join("land.txt");
    fs::write(&script, "SH echo hello > kid.txt\n").unwrap();
    let dir = verdicts(
        &f,
        &[
            r#"{"approve": false, "findings": ["kid.txt must say hello"]}"#,
            r#"Fixed. {"approve": true}"#,
        ],
    );
    let p = parent(&f, delegating(&f, Some(&script)));
    let spawned = p
        .delegate
        .spawn(reviewed(
            held(&p.go, "echo hi > kid.txt"),
            "kid",
            format!("REPLY_SEQUENCE {}", dir.display()),
        ))
        .unwrap();
    assert_eq!(spawned.review.unwrap().harness, "gemini-cli");
    p.end_turn("say hi again");
    p.release();

    assert_eq!(settled(&f, "root"), BranchStatus::Ready);
    assert!(
        merged_into_root(&status(&f, "kid")),
        "{:?}",
        status(&f, "kid")
    );
    assert_eq!(f.git(&["show", "by/root:kid.txt"]), "hello\n");
    // The child was sent the findings, numbered.
    let fix = prompts(&f, "kid").pop().unwrap();
    assert!(
        fix.contains(
            "your reviewer on gemini-cli asked for changes before it lands:\n\n\
             1. kid.txt must say hello"
        ),
        "{fix}"
    );
    // Two reviews, each a child of root, set aside after; the first saw the
    // task and the diff.
    let names = reviewers(&f, "kid");
    assert_eq!(names.len(), 2, "{names:?}");
    for name in &names {
        let info = f.yard.branch(name).unwrap().info().clone();
        assert!(
            matches!(info.status, BranchStatus::Discarded { .. }),
            "{name}: {:?}",
            info.status
        );
    }
    let asked = prompts(&f, &names[0]).remove(0);
    assert!(asked.contains("<branchyard-review>"), "{asked}");
    assert!(asked.contains("echo hi > kid.txt"), "the task: {asked}");
    assert!(asked.contains("+hi"), "the diff: {asked}");
    let outcomes: Vec<String> = landings(&f, "root").into_iter().map(|l| l.1).collect();
    assert!(
        outcomes[0].starts_with("its reviewer on gemini-cli asked for changes: kid.txt must"),
        "{outcomes:?}"
    );
    assert!(
        outcomes
            .iter()
            .any(|o| o.starts_with("its reviewer on gemini-cli approved it")),
        "{outcomes:?}"
    );
    assert!(outcomes.last().unwrap().starts_with("landed in by/root"));
    let landing = p.delegate.inspect("kid").unwrap().landing.unwrap();
    assert_eq!(landing.fixes, 1);
    // What was approved is the candidate that landed, not the merge.
    let kid = f.yard.branch("kid").unwrap().info().clone();
    assert_eq!(landing.reviewed, Some(kid.candidate.unwrap().commit));
}

#[test]
fn a_review_with_no_verdict_leaves_the_child_for_its_parent() {
    let f = Fixture::new();
    let answer = f.dir.join("answer.txt");
    fs::write(&answer, "It looks fine to me.").unwrap();
    let p = parent(&f, delegating(&f, None));
    p.delegate
        .spawn(reviewed(
            held(&p.go, "echo hi > kid.txt"),
            "kid",
            format!("REPLY_FILE {}", answer.display()),
        ))
        .unwrap();
    p.end_turn("say hi again");
    p.release();

    assert_eq!(settled(&f, "root"), BranchStatus::NoChanges);
    assert_eq!(status(&f, "kid"), BranchStatus::Ready, "not landed");
    let landing = p.delegate.inspect("kid").unwrap().landing.unwrap();
    assert!(landing.left);
    let why = landing.note.unwrap();
    assert!(
        why.starts_with("its review could not be done: ")
            && why.contains("gave no verdict: its answer has no JSON object"),
        "{why}"
    );
    let wake = prompts(&f, "root").pop().unwrap();
    assert!(
        wake.contains("Left for you, not landed: kid (its review could not be done"),
        "{wake}"
    );
}

#[test]
fn a_review_is_refused_without_land_or_a_harness_the_parent_may_run() {
    let f = Fixture::new();
    let p = parent(&f, delegating(&f, None));
    let review = ReviewSpec {
        harness: "gemini-cli".into(),
        ..ReviewSpec::default()
    };
    let unlanded = p.delegate.spawn(Spawn {
        prompt: "say hi".into(),
        name: Some("a".into()),
        review: Some(review.clone()),
        ..Spawn::default()
    });
    assert!(
        matches!(&unlanded, Err(branchyard::Error::Denied(why)) if why.contains("gates landing")),
        "{unlanded:?}"
    );
    let elsewhere = p.delegate.spawn(Spawn {
        review: Some(ReviewSpec {
            harness: "codex".into(),
            ..review
        }),
        ..landing("say hi".into(), "b")
    });
    assert!(
        matches!(&elsewhere, Err(branchyard::Error::Denied(why))
            if why.contains("may not delegate to") && why.contains("so it may not review with it")),
        "{elsewhere:?}"
    );
    assert!(f.yard.branch("a").is_err() && f.yard.branch("b").is_err());
}
