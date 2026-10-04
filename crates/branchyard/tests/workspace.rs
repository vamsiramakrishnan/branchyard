//! The workspace lifecycle against the fake ACP agent: files copied into a
//! new worktree (and what is refused), setup before the first turn, its
//! failure, the variables scripts and harness get, a port stable across
//! turns and a reopened store and distinct between branches, teardown on
//! removal, a yard that denies scripts, and setup recovered after its
//! engine is killed mid-setup (a real SIGKILL of a child process).

#![allow(clippy::let_underscore_must_use, clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use branchyard::{
    Activity, BranchStatus, RecordedEvent, TaskOptions, WorkspacePhase, WorkspaceReport,
    WorkspaceSpec, Yard,
};
use branchyard_testkit::wait;
use common::{fake_agent, text, Fixture};

fn reports(events: &[RecordedEvent]) -> Vec<WorkspaceReport> {
    events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Workspace(report) => Some(report.clone()),
            _ => None,
        })
        .collect()
}

fn prompts(events: &[RecordedEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e.activity, Activity::Prompt(_)))
        .count()
}

fn with(f: &Fixture, spec: WorkspaceSpec) -> TaskOptions {
    TaskOptions {
        workspace: Some(spec),
        ..f.options()
    }
}

/// `NAME=value` from the fake agent's `ENV` reply.
fn reported(text: &str, name: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix(&format!("{name}=")))
        .map(str::to_owned)
}

#[test]
fn untracked_files_are_copied_and_kept_out_of_candidates_and_escapes_refused() {
    let f = Fixture::new();
    // Tracked in the base: the worktree keeps the branch's own copy.
    fs::write(f.root.join(".env.example"), "EXAMPLE=committed\n").unwrap();
    fs::write(f.root.join(".gitignore"), ".env\n").unwrap();
    f.git(&["add", "."]);
    f.git(&["commit", "-q", "-m", "example"]);
    fs::write(f.root.join(".env.example"), "EXAMPLE=local edit\n").unwrap();
    fs::write(f.root.join(".env"), "SECRET=ignored\n").unwrap();
    // Untracked and not ignored: copied, and still never committed.
    fs::write(f.root.join(".env.local"), "LOCAL=1\n").unwrap();
    fs::create_dir_all(f.root.join("config/deep")).unwrap();
    fs::write(f.root.join("config/deep/app.local.json"), "{}\n").unwrap();
    symlink("/etc/hostname", f.root.join(".env.link")).unwrap();
    let outside = f.dir.join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("stolen.json"), "{}\n").unwrap();
    symlink(&outside, f.root.join("linked")).unwrap();

    let spec = WorkspaceSpec {
        copy: vec![
            ".env*".into(),
            "config".into(),
            "linked/*.json".into(),
            "../outside/*".into(),
        ],
        ..WorkspaceSpec::default()
    };
    let branch = f
        .yard
        .task("WRITE new.txt=1")
        .options(with(&f, spec))
        .name("copies")
        .run()
        .unwrap();
    // A refused glob fails the branch before its harness starts; what
    // could be copied was.
    let BranchStatus::Failed { reason } = &branch.info().status else {
        panic!("{:?}", branch.info().status);
    };
    assert!(reason.contains("copying files"), "{reason}");
    let worktree = &branch.info().worktree;
    assert_eq!(
        fs::read_to_string(worktree.join(".env")).unwrap(),
        "SECRET=ignored\n"
    );
    assert_eq!(
        fs::read_to_string(worktree.join(".env.local")).unwrap(),
        "LOCAL=1\n"
    );
    assert!(worktree.join("config/deep/app.local.json").is_file());
    assert_eq!(
        fs::read_to_string(worktree.join(".env.example")).unwrap(),
        "EXAMPLE=committed\n",
        "a tracked file is the branch's own"
    );
    assert!(!worktree.join(".env.link").exists());
    assert!(!worktree.join("linked").exists());

    let events = branch.events().unwrap();
    let copy = &reports(&events)[0];
    assert_eq!(copy.phase, WorkspacePhase::Copy);
    assert!(!copy.ok, "a refused glob fails the copy");
    assert_eq!(copy.copied, [".env", ".env.local", "config"]);
    let refused = copy.refused.join("\n");
    assert!(
        refused.contains(".env.link: is a symbolic link"),
        "{refused}"
    );
    assert!(refused.contains("linked/stolen.json"), "{refused}");
    assert!(
        refused.contains("../outside/*: must not leave"),
        "{refused}"
    );
    assert_eq!(prompts(&events), 0);

    // Without the escaping glob, the branch runs and its candidate holds
    // only the harness's work.
    let spec = WorkspaceSpec {
        copy: vec![".env*".into(), "config".into()],
        ..WorkspaceSpec::default()
    };
    let branch = f
        .yard
        .task("WRITE new.txt=1")
        .options(with(&f, spec))
        .name("copies-ok")
        .run()
        .unwrap();
    assert_eq!(branch.info().status, BranchStatus::Ready);
    let candidate = branch.info().candidate.clone().unwrap();
    let files = f.git(&[
        "diff",
        "--name-only",
        &branch.info().base,
        &candidate.commit,
    ]);
    assert_eq!(
        files.trim(),
        "new.txt",
        "copied files never reach a candidate"
    );
    let info = f.yard.workspace("copies-ok").unwrap();
    assert!(info.ready);
    assert_eq!(info.copied, [".env", ".env.local", "config"]);
}

#[test]
fn a_refused_copy_fails_the_branch_before_its_harness_starts() {
    let f = Fixture::new();
    let spec = WorkspaceSpec {
        copy: vec!["/etc/passwd".into()],
        setup: vec!["touch never-ran".into()],
        ..WorkspaceSpec::default()
    };
    let branch = f
        .yard
        .task("WRITE x.txt=1")
        .options(with(&f, spec))
        .run()
        .unwrap();
    let BranchStatus::Failed { reason } = &branch.info().status else {
        panic!("{:?}", branch.info().status);
    };
    assert!(reason.contains("workspace setup failed"), "{reason}");
    assert_eq!(prompts(&branch.events().unwrap()), 0);
    assert!(!branch.info().worktree.join("never-ran").exists());
}

#[test]
fn setup_runs_before_the_first_turn_with_the_branch_variables() {
    let f = Fixture::new();
    let spec = WorkspaceSpec {
        setup: vec![
            "printf '%s\\n' \"$BRANCHYARD_BRANCH\" \"$BRANCHYARD_WORKTREE\" \"$BRANCHYARD_ROOT\" \
             \"$BRANCHYARD_PORT\" \"${BRANCHYARD_DELEGATION-unset}\" > setup.env"
                .into(),
            "echo installed; mkdir -p node_modules && touch node_modules/.ok".into(),
        ],
        ..WorkspaceSpec::default()
    };
    let branch = f
        .yard
        .task("ENV BRANCHYARD_PORT BRANCHYARD_WORKTREE BRANCHYARD_BRANCH")
        .options(with(&f, spec))
        .name("envs")
        .run()
        .unwrap();
    assert_eq!(branch.info().status, BranchStatus::Ready);
    let worktree = branch.info().worktree.clone();
    let lines: Vec<String> = fs::read_to_string(worktree.join("setup.env"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(lines[0], "envs");
    assert_eq!(Path::new(&lines[1]), worktree);
    assert_eq!(Path::new(&lines[2]), f.root);
    let port: u16 = lines[3].parse().unwrap();
    assert!((20000..=29999).contains(&port));
    assert_eq!(lines[4], "unset", "a script gets no delegation token");
    assert!(worktree.join("node_modules/.ok").is_file());

    let events = branch.events().unwrap();
    let setup = reports(&events)
        .into_iter()
        .find(|r| r.phase == WorkspacePhase::Setup)
        .unwrap();
    assert!(setup.ok);
    assert_eq!(setup.commands.len(), 2);
    assert!(setup.output.contains("installed"));
    assert_eq!(setup.port, Some(port));
    // The setup event comes before the harness's first prompt.
    let setup_at = events
        .iter()
        .position(|e| matches!(e.activity, Activity::Workspace(_)))
        .unwrap();
    let prompt_at = events
        .iter()
        .position(|e| matches!(e.activity, Activity::Prompt(_)))
        .unwrap();
    assert!(setup_at < prompt_at);
    // The harness sees the same variables.
    let said = text(&events);
    assert_eq!(reported(&said, "BRANCHYARD_PORT"), Some(port.to_string()));
    assert_eq!(
        reported(&said, "BRANCHYARD_WORKTREE"),
        Some(worktree.display().to_string())
    );
    assert_eq!(reported(&said, "BRANCHYARD_BRANCH"), Some("envs".into()));
}

#[test]
fn a_failing_setup_fails_the_branch_with_its_output_and_runs_no_turn() {
    let f = Fixture::new();
    let spec = WorkspaceSpec {
        setup: vec![
            "echo resolving".into(),
            "echo 'npm ERR! missing script' >&2; exit 7".into(),
            "touch never-ran".into(),
        ],
        ..WorkspaceSpec::default()
    };
    let branch = f
        .yard
        .task("WRITE x.txt=1")
        .options(with(&f, spec))
        .name("broken")
        .run()
        .unwrap();
    let BranchStatus::Failed { reason } = &branch.info().status else {
        panic!("{:?}", branch.info().status);
    };
    assert!(reason.contains("exited with status 7"), "{reason}");
    assert!(reason.contains("exit 7"), "{reason}");
    let events = branch.events().unwrap();
    assert_eq!(prompts(&events), 0, "no turn ran");
    let setup = &reports(&events)[0];
    assert!(!setup.ok);
    assert_eq!(setup.exit_code, Some(7));
    assert!(setup.output.contains("resolving"));
    assert!(setup.output.contains("npm ERR! missing script"));
    assert!(!branch.info().worktree.join("never-ran").exists());
    assert!(!f.yard.workspace("broken").unwrap().ready);
}

#[test]
fn the_port_is_stable_across_turns_and_restarts_and_distinct_between_branches() {
    let f = Fixture::new();
    let spec = WorkspaceSpec {
        setup: vec!["echo $BRANCHYARD_PORT >> $BRANCHYARD_ROOT/ports.log".into()],
        ..WorkspaceSpec::default()
    };
    let branches: Vec<_> = ["one", "two"]
        .into_iter()
        .map(|name| {
            f.yard
                .task("ENV BRANCHYARD_PORT")
                .options(with(&f, spec.clone()))
                .name(name)
                .run()
                .unwrap()
        })
        .collect();
    let ports: Vec<String> = branches
        .iter()
        .map(|b| reported(&text(&b.events().unwrap()), "BRANCHYARD_PORT").unwrap())
        .collect();
    assert_ne!(ports[0], ports[1], "two branches never share a port");

    let first = &branches[0];
    let name = first.info().name.clone();
    let sent = first.send("ENV BRANCHYARD_PORT", f.options()).unwrap();
    let said = text(&sent.events().unwrap());
    let all: Vec<&str> = said
        .lines()
        .filter_map(|l| l.strip_prefix("BRANCHYARD_PORT="))
        .collect();
    assert_eq!(all, [ports[0].as_str(), ports[0].as_str()]);

    // A new engine on the same store: the same port, and setup is not run
    // again for a branch whose setup completed.
    let reopened = Yard::open(&f.root).unwrap();
    let again = reopened
        .branch(&name)
        .unwrap()
        .send("ENV BRANCHYARD_PORT", f.options())
        .unwrap();
    let said = text(&again.events().unwrap());
    assert_eq!(
        said.matches(&format!("BRANCHYARD_PORT={}", ports[0]))
            .count(),
        3
    );
    assert_eq!(
        reopened
            .workspace(&name)
            .unwrap()
            .port
            .map(|p| p.to_string()),
        Some(ports[0].clone())
    );
    let env = reopened.workspace_env(&name).unwrap();
    assert!(env.contains(&("BRANCHYARD_PORT".to_owned(), ports[0].clone())));
    let setups = fs::read_to_string(f.root.join("ports.log")).unwrap();
    assert_eq!(setups.lines().count(), 2, "one setup per branch: {setups}");

    // A fork is a new worktree: it gets its own setup and port.
    let written = again.send("WRITE f.txt=1", f.options()).unwrap();
    let fork = written
        .fork("ENV BRANCHYARD_PORT", true, f.options())
        .unwrap();
    let fork_port = reported(&text(&fork.events().unwrap()), "BRANCHYARD_PORT").unwrap();
    assert!(!ports.contains(&fork_port));
    assert_eq!(
        fs::read_to_string(f.root.join("ports.log"))
            .unwrap()
            .lines()
            .count(),
        3
    );
    assert!(f.yard.workspace(&fork.info().name).unwrap().spec.is_some());
}

#[test]
fn teardown_runs_on_removal_and_the_port_is_released() {
    let f = Fixture::new();
    let marker = f.dir.join("torn-down");
    let spec = WorkspaceSpec {
        setup: vec!["true".into()],
        teardown: vec![
            format!(
                "echo \"$BRANCHYARD_BRANCH $BRANCHYARD_PORT $(pwd)\" > {}",
                marker.display()
            ),
            "echo cleanup failed >&2; exit 2".into(),
        ],
        ..WorkspaceSpec::default()
    };
    let branch = f
        .yard
        .task("WRITE a.txt=changed")
        .options(with(&f, spec))
        .name("gone")
        .run()
        .unwrap();
    let port = f.yard.workspace("gone").unwrap().port.unwrap();
    let worktree = branch.info().worktree.clone();
    let report = f
        .yard
        .remove_reporting("gone", &Default::default())
        .unwrap()
        .expect("a teardown ran");
    assert_eq!(report.phase, WorkspacePhase::Teardown);
    assert!(!report.ok, "a failed teardown is reported");
    assert_eq!(report.exit_code, Some(2));
    assert!(report.output.contains("cleanup failed"));
    assert_eq!(
        fs::read_to_string(&marker).unwrap().trim(),
        format!("gone {port} {}", worktree.display())
    );
    assert!(!worktree.exists(), "the removal went on");
    assert!(f.yard.branch("gone").is_err());
    let db = rusqlite::Connection::open(f.root.join(".branchyard/state.db")).unwrap();
    let held: i64 = db
        .query_row("SELECT COUNT(*) FROM ports", [], |r| r.get(0))
        .unwrap();
    assert_eq!(held, 0, "the port was released");
    // The teardown is in the feed, which outlives the branch.
    let feed = f.yard.events_since(0, 1000).unwrap();
    assert!(feed.events.iter().any(|e| e.branch == "gone"
        && matches!(&e.event.activity, Activity::Workspace(r) if r.phase == WorkspacePhase::Teardown)));
}

#[test]
fn a_yard_that_denies_scripts_fails_setup_and_skips_teardown() {
    let f = Fixture::new();
    fs::write(f.root.join(".env"), "A=1\n").unwrap();
    let yard = Yard::open(&f.root).unwrap();
    yard.deny_workspace_scripts();
    let spec = WorkspaceSpec {
        copy: vec![".env".into()],
        setup: vec!["touch ran".into()],
        teardown: vec!["touch torn".into()],
        ..WorkspaceSpec::default()
    };
    let branch = yard
        .task("WRITE x.txt=1")
        .options(with(&f, spec))
        .name("denied")
        .run()
        .unwrap();
    let BranchStatus::Failed { reason } = &branch.info().status else {
        panic!("{:?}", branch.info().status);
    };
    assert!(reason.contains("allow_workspace_scripts"), "{reason}");
    let worktree = branch.info().worktree.clone();
    assert!(worktree.join(".env").is_file(), "copying is not a script");
    assert!(!worktree.join("ran").exists());
    let report = yard
        .remove_reporting("denied", &Default::default())
        .unwrap()
        .unwrap();
    assert!(!report.ok && report.commands.is_empty());
    assert!(!worktree.exists());
}

/// Run a turn whose setup is in `BY_WS_SETUP` on a branch named `slow`, in
/// the repository at `BY_WS_ROOT`. Run only as the child of the crash test,
/// which kills it.
#[test]
#[ignore = "the child process of the setup crash test"]
fn workspace_child() {
    let (Some(root), Some(setup), Some(agent)) = (
        std::env::var_os("BY_WS_ROOT"),
        std::env::var("BY_WS_SETUP").ok(),
        std::env::var("BY_WS_AGENT").ok(),
    ) else {
        return;
    };
    let yard = Yard::open(root).unwrap();
    let _ = yard
        .task("WRITE done.txt=1")
        .options(TaskOptions {
            harness: Some("gemini-cli".into()),
            command: Some(vec![agent]),
            workspace: Some(WorkspaceSpec {
                setup: vec![setup],
                ..WorkspaceSpec::default()
            }),
            ..TaskOptions::default()
        })
        .name("slow")
        .run();
}

struct Killed(Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn running(pid: u32) -> bool {
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    out.status.success() && !stat.trim().is_empty() && !stat.trim().starts_with('Z')
}

#[test]
fn setup_cut_short_by_a_killed_engine_is_recovered_and_runs_again() {
    let f = Fixture::new();
    let log = f.dir.join("setup.log");
    let once = f.dir.join("once");
    // The first attempt records its sleeper's pid and hangs; the second
    // finishes at once.
    let setup = format!(
        "echo attempt >> {log}; if [ ! -e {once} ]; then touch {once}; sleep 120 & echo $! > \
         {pids}; wait; fi",
        log = log.display(),
        once = once.display(),
        pids = f.dir.join("sleeper").display()
    );
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "workspace_child",
            "--exact",
            "--ignored",
            "--test-threads=1",
        ])
        .env("BY_WS_ROOT", &f.root)
        .env("BY_WS_AGENT", fake_agent())
        .env("BY_WS_SETUP", &setup)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut child = Killed(child);
    let pid_file = f.dir.join("sleeper");
    wait::until("setup to start its sleeper", || {
        fs::read_to_string(&pid_file).is_ok_and(|t| t.trim().parse::<u32>().is_ok())
    });
    let sleeper: u32 = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(running(sleeper));
    child.0.kill().unwrap();
    child.0.wait().unwrap();

    let yard = Yard::open(&f.root).unwrap();
    let branch = yard.branch("slow").unwrap();
    assert_eq!(branch.info().status, BranchStatus::Interrupted);
    let events = branch.events().unwrap();
    let recovered: Vec<(String, Vec<u32>)> = events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Recovered { reason, killed } => Some((reason.clone(), killed.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(recovered.len(), 1, "{events:?}");
    let (reason, killed) = &recovered[0];
    assert!(
        reason.contains("before the harness was started; the turn never ran"),
        "{reason}"
    );
    assert!(
        reason.contains("its workspace setup was cut short and runs again"),
        "{reason}"
    );
    assert!(killed.contains(&sleeper), "{killed:?}");
    wait::until("the setup's sleeper to die", || !running(sleeper));
    assert_eq!(prompts(&events), 0);
    assert!(!yard.workspace("slow").unwrap().ready);

    // The next turn runs setup again, from the start, then the prompt.
    let sent = branch.send("WRITE done.txt=1", f.options()).unwrap();
    assert_eq!(sent.info().status, BranchStatus::Ready);
    assert_eq!(fs::read_to_string(&log).unwrap().lines().count(), 2);
    assert!(yard.workspace("slow").unwrap().ready);
    assert!(sent.info().worktree.join("done.txt").is_file());
}

#[test]
fn a_delegated_child_gets_its_parents_workspace_in_its_own_worktree() {
    use branchyard::{Envelope, Policy, Spawn};
    let f = Fixture::new();
    fs::write(f.root.join(".env"), "A=1\n").unwrap();
    let options = TaskOptions {
        delegation: Some(Envelope::default()),
        delegation_server: Some(vec![fake_agent().display().to_string()]),
        policy: Policy::allow_all(),
        workspace: Some(WorkspaceSpec {
            copy: vec![".env".into()],
            setup: vec!["echo $BRANCHYARD_BRANCH >> $BRANCHYARD_ROOT/setups.log".into()],
            ..WorkspaceSpec::default()
        }),
        ..f.options()
    };
    let root = f
        .yard
        .task("WRITE root.txt=r")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let delegate = root.delegate(options).unwrap();
    delegate
        .spawn(Spawn {
            prompt: "WRITE kid.txt=k".into(),
            name: Some("kid".into()),
            ..Spawn::default()
        })
        .unwrap();
    let finished = root.wait_subtree().unwrap();
    assert_eq!(finished[0].status, BranchStatus::Ready);
    assert_eq!(
        fs::read_to_string(f.root.join("setups.log")).unwrap(),
        "root\nkid\n"
    );
    let kid = f.yard.workspace("kid").unwrap();
    assert!(kid.ready && kid.worktree.join(".env").is_file());
    assert_ne!(kid.port, f.yard.workspace("root").unwrap().port);
}

#[test]
fn a_copied_directory_the_branch_also_tracks_never_hides_the_agents_edits() {
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("config")).unwrap();
    fs::write(f.root.join("config/tracked.txt"), "old\n").unwrap();
    f.git(&["add", "."]);
    f.git(&["commit", "-q", "-m", "config"]);
    fs::write(f.root.join("config/local.json"), "{}\n").unwrap();
    fs::create_dir_all(f.root.join("cache/deep")).unwrap();
    fs::write(f.root.join("cache/deep/blob"), "x\n").unwrap();
    let spec = WorkspaceSpec {
        copy: vec!["config".into(), "cache".into()],
        ..WorkspaceSpec::default()
    };
    let branch = f
        .yard
        .task("WRITE config/tracked.txt=new")
        .options(with(&f, spec))
        .name("tracked-dir")
        .run()
        .unwrap();
    assert_eq!(branch.info().status, BranchStatus::Ready);
    // Only a directory the branch tracks nothing in is named whole.
    assert_eq!(
        f.yard.workspace("tracked-dir").unwrap().copied,
        ["cache", "config/local.json"]
    );
    let candidate = branch.info().candidate.clone().unwrap();
    let files = f.git(&[
        "diff",
        "--name-only",
        &branch.info().base,
        &candidate.commit,
    ]);
    assert_eq!(files.trim(), "config/tracked.txt");
}
