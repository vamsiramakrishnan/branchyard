//! Tasks and their repositories (docs/task-repos.md), against the fake ACP
//! agent: the task's record committed beside each checkpoint and never in
//! merges and diffs; folder tasks that leave the folder alone until an
//! attempt is accepted, then apply it exactly, and refuse to overwrite what
//! changed outside; ignore rules; rewind and fork restoring files and the
//! conversation together; large files chunked, shared between tasks and
//! restored byte for byte; and what sync reads.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use branchyard::tasks::{self, NewTask, Pointer, TaskFiles, TaskRepo};
use branchyard::{BranchStatus, Error, Policy, TaskOptions, Yard};
use common::{git, Fixture};

fn allow(f: &Fixture) -> TaskOptions {
    TaskOptions {
        policy: Policy::allow_all(),
        ..f.options()
    }
}

/// Every file under `dir`, with its bytes, by relative path.
fn files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let rel = path.strip_prefix(root).unwrap().display().to_string();
                out.insert(rel, fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

/// Deterministic bytes that do not compress or repeat.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

/// A folder to grant, with a home for task repositories beside it.
struct Granted {
    folder: PathBuf,
    home: PathBuf,
}

fn granted(f: &Fixture, contents: &[(&str, &[u8])]) -> Granted {
    let folder = f.dir.join("folder");
    for (path, bytes) in contents {
        let path = folder.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    fs::create_dir_all(&folder).unwrap();
    Granted {
        folder,
        home: f.dir.join("home"),
    }
}

fn new_task(g: &Granted, prompt: &str, folder: bool) -> NewTask {
    NewTask {
        prompt: prompt.into(),
        folder: folder.then(|| g.folder.clone()),
        policy: "allow the rest".into(),
        home: Some(g.home.clone()),
        ..NewTask::default()
    }
}

/// Run `prompt` as the first attempt of the task `yard` was made for.
fn attempt(f: &Fixture, yard: &Yard, id: &str, name: &str, prompt: &str) -> branchyard::Branch {
    yard.task(prompt)
        .options(TaskOptions {
            name: Some(name.into()),
            base: Some("main".into()),
            join_task: Some(id.into()),
            ..allow(f)
        })
        .run()
        .unwrap()
}

fn tree_paths(dir: &Path, rev: &str) -> Vec<String> {
    git(dir, &["ls-tree", "-r", "--name-only", rev])
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_run_is_a_task_whose_record_is_committed_and_kept_out_of_merges_and_diffs() {
    let f = Fixture::new();
    let branch = f
        .task("WRITE notes.txt=hello")
        .name("first")
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    assert_eq!(branch.info().status, BranchStatus::Ready);
    // One task, one attempt, its conversation committed.
    let list = tasks::list(&f.yard).unwrap();
    assert_eq!(list.len(), 1);
    let view = &list[0];
    assert_eq!(view.task.origin, "run");
    assert_eq!(view.task.asked, "WRITE notes.txt=hello");
    assert_eq!(view.task.files, TaskFiles::Repository);
    assert_eq!(view.task.policy, "allow the rest");
    assert_eq!(view.attempts.len(), 1);
    assert_eq!(view.attempts[0].name, "first");
    assert_eq!(view.attempts[0].conversation, ["1.jsonl"]);
    assert_eq!(tasks::find(&f.yard, "first").unwrap().id, view.task.id);
    assert_eq!(
        tasks::find(&f.yard, &view.task.id[..8].to_lowercase())
            .unwrap()
            .id,
        view.task.id
    );
    // The record commit of checkpoint 1: its files and `.task/`.
    let record = view.attempts[0].record.clone().unwrap();
    assert_eq!(
        f.git(&["rev-parse", "refs/branchyard/first/1/record-1"])
            .trim(),
        record
    );
    let parents = f.git(&["rev-list", "--parents", "-n", "1", &record]);
    let candidate = branch.info().candidate.clone().unwrap();
    assert!(parents.trim().ends_with(&candidate.commit), "{parents}");
    let paths = tree_paths(&f.root, &record);
    for path in [
        ".task/task.toml",
        ".task/conversation/1.jsonl",
        ".task/effects.jsonl",
        "notes.txt",
    ] {
        assert!(paths.contains(&path.to_owned()), "{paths:?}");
    }
    let toml = f.git(&["show", &format!("{record}:.task/task.toml")]);
    assert!(toml.contains("format = \"branchyard-task/1\""), "{toml}");
    assert!(
        toml.contains(&format!("id = \"{}\"", view.task.id)),
        "{toml}"
    );
    assert!(toml.contains("attempt = \"first\""), "{toml}");
    let turn = f.git(&["show", &format!("{record}:.task/conversation/1.jsonl")]);
    let summary: serde_json::Value = serde_json::from_str(turn.lines().next().unwrap()).unwrap();
    assert_eq!(summary["turn"], 1);
    assert_eq!(summary["prompt"], "WRITE notes.txt=hello");
    assert!(
        summary["answer"]
            .as_str()
            .unwrap()
            .contains("wrote notes.txt"),
        "{summary}"
    );
    assert!(turn.lines().count() > 2, "{turn}");
    // The branch and its candidate are as they always were.
    assert_eq!(f.git(&["rev-parse", "by/first"]).trim(), candidate.commit);
    assert!(!tree_paths(&f.root, "by/first")
        .iter()
        .any(|p| p.starts_with(".task")));
    assert_eq!(candidate.files_changed, 1);
    let diff = branch.diff().unwrap();
    assert!(
        diff.contains("notes.txt") && !diff.contains(".task"),
        "{diff}"
    );
    // A turn that changes no file keeps the candidate, ready; its record
    // has the conversation so far.
    let again = branch.send("just answer", allow(&f)).unwrap();
    assert_eq!(again.info().status, BranchStatus::Ready);
    assert_eq!(again.info().candidate, Some(candidate));
    let view = tasks::view(&f.yard, "first").unwrap();
    assert_eq!(view.attempts[0].conversation, ["1.jsonl", "2.jsonl"]);
    // Merged without it, in the tree and in history.
    let merged = f.yard.merge("first", "main").unwrap();
    let main = tree_paths(&f.root, "main");
    assert!(main.contains(&"notes.txt".to_owned()), "{main:?}");
    assert!(!main.iter().any(|p| p.starts_with(".task")), "{main:?}");
    assert_eq!(f.git(&["log", "--format=%H", "main", "--", ".task"]), "");
    assert_eq!(f.git(&["rev-parse", "main"]).trim(), merged.commit);
    // The record stays, beside the checkpoints.
    assert!(tree_paths(&f.root, "refs/branchyard/first/1/record-2")
        .contains(&".task/conversation/2.jsonl".to_owned()));
}

#[test]
fn a_fan_is_one_task_and_a_fork_joins_it() {
    let f = Fixture::new();
    let fan = f
        .task("WRITE f.txt=fan")
        .name("fan")
        .policy(Policy::allow_all())
        .run_on(&["gemini-cli", "qwen-code"])
        .unwrap();
    assert_eq!(fan.len(), 2);
    let list = tasks::list(&f.yard).unwrap();
    assert_eq!(list.len(), 1, "{list:?}");
    assert_eq!(list[0].task.origin, "fan");
    let names: Vec<&str> = list[0].attempts.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["fan-gemini-cli", "fan-qwen-code"]);
    let forked = fan[0]
        .fork(
            "WRITE g.txt=more",
            true,
            TaskOptions {
                name: Some("fan-more".into()),
                ..allow(&f)
            },
        )
        .unwrap();
    let view = tasks::view(&f.yard, "fan-more").unwrap();
    assert_eq!(view.task.id, list[0].task.id);
    assert_eq!(view.attempts.len(), 3);
    // The fork's turns are numbered after its parent's.
    assert_eq!(
        view.attempts[2].conversation,
        ["1.jsonl", "2.jsonl"],
        "{:?}",
        view.attempts[2]
    );
    assert_eq!(
        view.attempts[2].forked_from.as_deref(),
        Some("fan-gemini-cli")
    );
    assert!(forked.info().candidate.is_some());
    // Removing an attempt leaves it listed, as removed.
    f.yard.remove("fan-qwen-code").unwrap();
    let view = tasks::view(&f.yard, &list[0].task.id).unwrap();
    assert_eq!(view.attempts[1].status, None);
    // Removing the task removes its attempts.
    tasks::remove(&f.yard, &list[0].task.id).unwrap();
    assert!(tasks::list(&f.yard).unwrap().is_empty());
    assert!(f.yard.branches().unwrap().is_empty());
}

#[test]
fn rewind_and_fork_restore_the_files_and_the_conversation_together() {
    let f = Fixture::new();
    let mut branch = f
        .task("WRITE s.txt=one")
        .name("steps")
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    for prompt in ["WRITE s.txt=two", "WRITE s.txt=three"] {
        branch = branch.send(prompt, allow(&f)).unwrap();
    }
    let worktree = branch.info().worktree.clone();
    let conversation = |name: &str| {
        let view = tasks::view(&f.yard, name).unwrap();
        let attempt = view.attempts.iter().find(|a| a.name == name).unwrap();
        (
            attempt.record.clone().unwrap(),
            attempt.conversation.clone(),
        )
    };
    assert_eq!(conversation("steps").1, ["1.jsonl", "2.jsonl", "3.jsonl"]);
    branch.rewind(1).unwrap();
    assert_eq!(fs::read_to_string(worktree.join("s.txt")).unwrap(), "one\n");
    assert_eq!(conversation("steps").1, ["1.jsonl"]);
    // Forward again, both come back.
    branch.rewind(3).unwrap();
    assert_eq!(
        fs::read_to_string(worktree.join("s.txt")).unwrap(),
        "three\n"
    );
    assert_eq!(conversation("steps").1, ["1.jsonl", "2.jsonl", "3.jsonl"]);
    // A fork at checkpoint 2 has the first two turns, then its own.
    let fork = branch
        .fork_at(
            2,
            "WRITE t.txt=fork",
            TaskOptions {
                name: Some("steps-fork".into()),
                ..allow(&f)
            },
        )
        .unwrap();
    let fork_tree = fork.info().worktree.clone();
    assert_eq!(
        fs::read_to_string(fork_tree.join("s.txt")).unwrap(),
        "two\n"
    );
    let (record, names) = conversation("steps-fork");
    assert_eq!(names, ["1.jsonl", "2.jsonl", "3.jsonl"]);
    let show = |rev: &str, path: &str| f.git(&["show", &format!("{rev}:{path}")]);
    let third = show(&record, ".task/conversation/3.jsonl");
    assert!(third.contains("\"attempt\":\"steps-fork\""), "{third}");
    assert!(third.contains("WRITE t.txt=fork"), "{third}");
    let first = show(&record, ".task/conversation/1.jsonl");
    assert!(first.contains("\"attempt\":\"steps\""), "{first}");
    assert_eq!(show(&record, "s.txt"), "two\n");
    assert_eq!(show(&record, "t.txt"), "fork\n");
    // After a rewind, the next turn continues the record it went back to.
    branch.rewind(1).unwrap();
    let branch = branch.send("WRITE s.txt=four", allow(&f)).unwrap();
    assert_eq!(branch.info().turns, 4);
    let (record, names) = conversation("steps");
    assert_eq!(names, ["1.jsonl", "4.jsonl"]);
    assert_eq!(show(&record, "s.txt"), "four\n");
    assert_eq!(
        tasks::task_of(&f.root, "steps-fork").unwrap().unwrap().id,
        tasks::task_of(&f.root, "steps").unwrap().unwrap().id
    );
}

#[test]
fn a_folder_is_untouched_until_an_attempt_is_accepted_then_gets_exactly_its_change() {
    let f = Fixture::new();
    let g = granted(
        &f,
        &[
            ("a.txt", b"original\n"),
            ("sub/b.txt", b"keep\n"),
            ("old.txt", b"to remove\n"),
            ("node_modules/x.js", b"cache\n"),
            (".DS_Store", b"os\n"),
            (".gitignore", b"*.log\n"),
            ("debug.log", b"ignored by the folder\n"),
            (".branchyardignore", b"secret.txt\n"),
            ("secret.txt", b"never committed\n"),
        ],
    );
    let before = files(&g.folder);
    let (task, yard) = tasks::create(&new_task(&g, "tidy the folder", true)).unwrap();
    assert_eq!(
        task.files,
        TaskFiles::Folder {
            folder: fs::canonicalize(&g.folder).unwrap()
        }
    );
    // Ignore rules: the folder's .gitignore, .branchyardignore, defaults.
    let main = tree_paths(yard.root(), "main");
    assert_eq!(
        main,
        [
            ".branchyardignore",
            ".gitignore",
            "a.txt",
            "old.txt",
            "sub/b.txt"
        ],
        "{main:?}"
    );
    let branch = attempt(
        &f,
        &yard,
        &task.id,
        "tidy",
        "SH printf 'changed\\n' > a.txt\nSH mkdir -p new && printf 'created\\n' > new/c.txt\nSH rm old.txt",
    );
    assert_eq!(
        branch.info().status,
        BranchStatus::Ready,
        "{:?}",
        branch.info()
    );
    // The attempt ran in a worktree of the task's repository, not the folder.
    assert!(!branch.info().worktree.starts_with(&g.folder));
    assert_eq!(files(&g.folder), before, "the folder changed before accept");
    assert!(!g.folder.join(".git").exists() && !g.folder.join(".branchyard").exists());
    let view = tasks::view(&yard, &task.id).unwrap();
    assert_eq!(view.folder_at, view.accepted);
    let accepted = tasks::accept(&yard, &task.id, None, None).unwrap();
    assert_eq!(accepted.attempt, "tidy");
    assert_eq!(accepted.written, ["a.txt", "new/c.txt"]);
    assert_eq!(accepted.removed, ["old.txt"]);
    let mut expected = before.clone();
    expected.insert("a.txt".into(), b"changed\n".to_vec());
    expected.insert("new/c.txt".into(), b"created\n".to_vec());
    expected.remove("old.txt");
    assert_eq!(files(&g.folder), expected);
    // The attempt is merged, main and the folder's ref moved together.
    let view = tasks::view(&yard, &task.id).unwrap();
    assert_eq!(view.accepted.as_deref(), Some(accepted.commit.as_str()));
    assert_eq!(view.folder_at, view.accepted);
    assert!(matches!(
        view.attempts[0].status,
        Some(BranchStatus::Merged { .. })
    ));
    // main keeps the conversation; the folder never sees it.
    assert!(tree_paths(yard.root(), "main").contains(&".task/task.toml".to_owned()));
    assert!(matches!(
        tasks::accept(&yard, &task.id, Some("tidy"), None),
        Err(Error::AlreadyMerged { .. })
    ));
    // Listed among the tasks with a repository of their own, and removed
    // without touching the folder.
    let home = tasks::home_tasks(&g.home).unwrap();
    assert!(home.iter().any(|v| v.task.id == task.id));
    drop(yard);
    tasks::remove_home(&g.home, &task.id[..6]).unwrap();
    assert!(tasks::home_tasks(&g.home).unwrap().is_empty());
    assert_eq!(files(&g.folder), expected);
}

#[test]
fn accept_refuses_to_overwrite_a_file_changed_outside_the_task() {
    let f = Fixture::new();
    let g = granted(&f, &[("a.txt", b"original\n"), ("b.txt", b"b\n")]);
    let (task, yard) = tasks::create(&new_task(&g, "edit a", true)).unwrap();
    attempt(
        &f,
        &yard,
        &task.id,
        "edit",
        "WRITE a.txt=agent WRITE c.txt=new",
    );
    // The person edits the same file meanwhile, and creates c.txt.
    fs::write(g.folder.join("a.txt"), "person\n").unwrap();
    fs::write(g.folder.join("c.txt"), "theirs\n").unwrap();
    let before = files(&g.folder);
    let error = tasks::accept(&yard, &task.id, None, None).unwrap_err();
    let text = error.to_string();
    assert!(matches!(error, Error::Denied(_)), "{error:?}");
    assert!(text.contains("a.txt: changed in the folder"), "{text}");
    assert!(text.contains("c.txt: created in the folder"), "{text}");
    assert_eq!(files(&g.folder), before, "nothing may be written");
    let view = tasks::view(&yard, &task.id).unwrap();
    assert_eq!(view.attempts[0].status, Some(BranchStatus::Ready));
    // Put back as they were (a file already as the attempt has it counts as
    // applied), and it goes through.
    fs::write(g.folder.join("a.txt"), "original\n").unwrap();
    fs::write(g.folder.join("c.txt"), "new\n").unwrap();
    let accepted = tasks::accept(&yard, &task.id, None, None).unwrap();
    assert_eq!(accepted.written, ["a.txt"]);
    assert_eq!(
        fs::read_to_string(g.folder.join("a.txt")).unwrap(),
        "agent\n"
    );
    assert_eq!(fs::read_to_string(g.folder.join("b.txt")).unwrap(), "b\n");
}

#[test]
fn large_files_are_chunked_shared_between_tasks_and_restored_byte_for_byte() {
    let f = Fixture::new();
    let big = noise(3 * 1024 * 1024, 5);
    let g = granted(&f, &[("big.bin", &big), ("small.txt", b"small\n")]);
    let chunks = f.dir.join("chunks");
    let large = |prompt: &str| NewTask {
        large_threshold: Some(256 * 1024),
        chunks: Some(chunks.clone()),
        ..new_task(&g, prompt, true)
    };
    let (task, yard) = tasks::create(&large("first")).unwrap();
    // git holds a pointer, not the bytes.
    let pointer = git(yard.root(), &["show", "main:big.bin"]);
    let pointer = Pointer::parse(pointer.as_bytes()).expect("a pointer");
    assert_eq!(pointer.size, big.len() as u64);
    assert!(pointer.chunks.len() > 2, "{pointer:?}");
    let stored = |dir: &Path| {
        fs::read_dir(dir)
            .map(|d| {
                d.flat_map(|e| fs::read_dir(e.unwrap().path()).unwrap())
                    .count()
            })
            .unwrap_or(0)
    };
    assert_eq!(stored(&chunks), pointer.chunks.len());
    // A second task over the same content stores nothing new.
    let (second, second_yard) = tasks::create(&large("second")).unwrap();
    assert_eq!(stored(&chunks), pointer.chunks.len());
    assert_eq!(
        git(second_yard.root(), &["show", "main:big.bin"]),
        pointer.render()
    );
    // reachable_chunks names exactly the pointer's chunks.
    let repo = TaskRepo::open(&g.home, &task.id).unwrap();
    assert!(repo.own);
    let reachable = repo.reachable_chunks("main").unwrap();
    let named: std::collections::BTreeSet<String> =
        pointer.chunks.iter().map(|(h, _)| h.clone()).collect();
    assert_eq!(reachable, named);
    assert_eq!(
        tasks::reachable_chunks(&repo.git_dir, "main").unwrap(),
        named
    );
    assert!(repo
        .refs()
        .unwrap()
        .iter()
        .any(|(name, _)| name == "refs/heads/main"));
    assert_eq!(TaskRepo::list(&g.home).unwrap().len(), 2);
    drop(second_yard);
    tasks::remove_home(&g.home, &second.id).unwrap();

    // An attempt's worktree gets the real bytes; two turns change it.
    let v2 = noise(3 * 1024 * 1024 + 4096, 6);
    let v3 = noise(2 * 1024 * 1024, 7);
    fs::write(f.dir.join("v2.bin"), &v2).unwrap();
    fs::write(f.dir.join("v3.bin"), &v3).unwrap();
    let branch = attempt(&f, &yard, &task.id, "big", "WRITE note.txt=start");
    let worktree = branch.info().worktree.clone();
    assert_eq!(fs::read(worktree.join("big.bin")).unwrap(), big);
    assert_eq!(git(&worktree, &["status", "--porcelain"]), "");
    let branch = branch
        .send(
            &format!("SH cp {} big.bin", f.dir.join("v2.bin").display()),
            allow(&f),
        )
        .unwrap();
    assert_eq!(branch.info().status, BranchStatus::Ready);
    let branch = branch
        .send(
            &format!("SH cp {} big.bin", f.dir.join("v3.bin").display()),
            allow(&f),
        )
        .unwrap();
    let head = git(&worktree, &["show", "HEAD:big.bin"]);
    assert_eq!(
        Pointer::parse(head.as_bytes()).unwrap().size,
        v3.len() as u64
    );
    // An edit since the last checkpoint is seen, and refuses a rewind.
    fs::write(worktree.join("big.bin"), b"edited by hand").unwrap();
    let refused = branch.rewind(2).unwrap_err();
    assert!(refused.to_string().contains("big.bin"), "{refused}");
    fs::write(worktree.join("big.bin"), &v3).unwrap();
    branch.rewind(2).unwrap();
    assert_eq!(fs::read(worktree.join("big.bin")).unwrap(), v2);
    assert_eq!(git(&worktree, &["status", "--porcelain"]), "");
    // Accepting writes the bytes into the folder.
    let accepted = tasks::accept(&yard, &task.id, None, None).unwrap();
    assert!(
        accepted.written.contains(&"big.bin".to_owned()),
        "{accepted:?}"
    );
    assert_eq!(fs::read(g.folder.join("big.bin")).unwrap(), v2);
    assert_eq!(
        fs::read_to_string(g.folder.join("note.txt")).unwrap(),
        "start\n"
    );
    let all = repo.reachable_chunks("main").unwrap();
    assert!(all.is_superset(&named));
    assert!(all.len() > named.len());
}

#[test]
fn a_task_with_no_files_keeps_its_conversation_and_results() {
    let f = Fixture::new();
    let g = granted(&f, &[]);
    let (task, yard) = tasks::create(&new_task(&g, "draft a message", false)).unwrap();
    assert_eq!(task.files, TaskFiles::NoFiles);
    assert!(tree_paths(yard.root(), "main").is_empty());
    let branch = attempt(&f, &yard, &task.id, "draft", "WRITE message.md=hello");
    assert_eq!(branch.info().status, BranchStatus::Ready);
    let accepted = tasks::accept(&yard, &task.id, None, None).unwrap();
    assert_eq!(accepted.folder, None);
    let main = tree_paths(yard.root(), "main");
    assert!(main.contains(&"message.md".to_owned()), "{main:?}");
    assert!(
        main.contains(&".task/conversation/1.jsonl".to_owned()),
        "{main:?}"
    );
    // An answer with no file still has a repository: its conversation.
    let research = attempt(&f, &yard, &task.id, "ask", "what is two and two");
    assert_eq!(research.info().status, BranchStatus::NoChanges);
    let view = tasks::view(&yard, &task.id).unwrap();
    assert_eq!(view.attempts.len(), 2);
    assert_eq!(view.attempts[1].conversation, ["1.jsonl"]);
}

#[test]
fn a_repository_tasks_refs_are_its_attempts() {
    let f = Fixture::new();
    f.task("WRITE a.txt=x")
        .name("one")
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    f.task("WRITE b.txt=x")
        .name("one-more")
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    let repo = TaskRepo::of(&f.yard, "one").unwrap();
    assert!(!repo.own);
    let refs: Vec<String> = repo.refs().unwrap().into_iter().map(|(n, _)| n).collect();
    assert_eq!(
        refs,
        [
            "refs/branchyard/one/1/record-1".to_owned(),
            "refs/branchyard/one/1/turn-1".to_owned(),
            "refs/heads/by/one".to_owned()
        ],
        "{refs:?}"
    );
    assert!(repo.reachable_chunks("by/one").unwrap().is_empty());
}

#[test]
fn a_second_attempt_is_merged_into_what_the_first_one_left() {
    let f = Fixture::new();
    let g = granted(&f, &[("a.txt", b"a\n"), ("b.txt", b"b\n")]);
    let (task, yard) = tasks::create(&new_task(&g, "two ways", true)).unwrap();
    attempt(&f, &yard, &task.id, "one", "WRITE a.txt=first");
    attempt(&f, &yard, &task.id, "two", "WRITE b.txt=second");
    tasks::accept(&yard, &task.id, Some("one"), None).unwrap();
    // Both records have a turn 1; the accepted attempt's is kept.
    let second = tasks::accept(&yard, &task.id, Some("two"), None).unwrap();
    assert_eq!(second.written, ["b.txt"]);
    assert_eq!(
        fs::read_to_string(g.folder.join("a.txt")).unwrap(),
        "first\n"
    );
    assert_eq!(
        fs::read_to_string(g.folder.join("b.txt")).unwrap(),
        "second\n"
    );
    let main = git(yard.root(), &["show", "main:.task/task.toml"]);
    assert!(main.contains("attempt = \"two\""), "{main}");
    let parents = git(yard.root(), &["rev-list", "--parents", "-n", "1", "main"]);
    assert_eq!(parents.split_whitespace().count(), 3, "{parents}");
}
