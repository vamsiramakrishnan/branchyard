//! `by map` through the built `by`, against temporary repositories and the
//! fake ACP agent: items from each input format, standard input and a
//! command; template substitution; answers checked against a schema, an
//! invalid one fixed by the follow-up turn, an item failing after its
//! retries; the concurrency bound; resuming an interrupted map; CSV and
//! JSON lines results; the reduce; the total budget; routing with
//! failover; `by map ls|show|rm`, `by ls` and `by show`; and the same map
//! through `by serve` with `--remote`. Hermetic: no model is called.
//! Requires `git`, `sh` and `kill`.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::Value;

static COUNTER: AtomicU64 = AtomicU64::new(0);

const BY: &str = env!("CARGO_BIN_EXE_by");

fn fake_agent() -> &'static Path {
    static AGENT: OnceLock<PathBuf> = OnceLock::new();
    AGENT.get_or_init(|| {
        let by = PathBuf::from(BY);
        let profile_dir = by.parent().unwrap().to_path_buf();
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let mut command = Command::new(cargo);
        command
            .args(["build", "--quiet", "--offline", "--manifest-path"])
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml"))
            .args(["-p", "branchyard-runtime", "--bin", "fake-acp-agent"])
            .env("CARGO_TARGET_DIR", profile_dir.parent().unwrap());
        match profile_dir.file_name().and_then(|n| n.to_str()) {
            Some("debug") => {}
            Some("release") => {
                command.arg("--release");
            }
            Some(other) => {
                command.args(["--profile", other]);
            }
            None => panic!("unexpected binary location {}", by.display()),
        }
        assert!(
            command.status().unwrap().success(),
            "building fake-acp-agent failed"
        );
        let agent = profile_dir.join("fake-acp-agent");
        assert!(agent.is_file());
        agent
    })
}

struct Repo {
    dir: PathBuf,
    root: PathBuf,
}

impl Repo {
    fn new() -> Repo {
        fake_agent();
        let dir = std::env::temp_dir().join(format!(
            "branchyard-cli-map-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("repo")).unwrap();
        let dir = fs::canonicalize(dir).unwrap();
        let repo = Repo {
            root: dir.join("repo"),
            dir,
        };
        repo.git(&["init", "-q", "-b", "main"]);
        repo.git(&["config", "user.name", "Test"]);
        repo.git(&["config", "user.email", "test@localhost"]);
        fs::write(repo.root.join("a.txt"), "one\n").unwrap();
        fs::write(repo.root.join(".gitignore"), "branchyard.toml\n").unwrap();
        repo.git(&["add", "."]);
        repo.git(&["commit", "-q", "-m", "initial"]);
        repo
    }

    /// A file outside the repository, with `text`; its path.
    fn file(&self, name: &str, text: &str) -> String {
        let path = self.dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, text).unwrap();
        path.display().to_string()
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(&self.root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("NO_COLOR", "1")
            .env("BRANCHYARD_USER_CONFIG", self.dir.join("user/config.toml"))
            .env("PAGER", "cat");
        for var in [
            "BRANCHYARD_DELEGATION",
            "BRANCHYARD_BRANCH",
            "BRANCHYARD_ROOT",
            "BRANCHYARD_BY",
            "BRANCHYARD_REMOTE",
            "BRANCHYARD_TOKEN_FILE",
            "BRANCHYARD_REPO",
        ] {
            command.env_remove(var);
        }
        command
    }

    fn git(&self, args: &[&str]) -> String {
        let out = self.command("git").args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    fn by(&self, args: &[&str]) -> Output {
        self.command(BY)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn by_with_stdin(&self, args: &[&str], input: &str) -> Output {
        let mut child = self
            .command(BY)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> Output {
        let out = self.by(args);
        assert!(
            out.status.success(),
            "by {args:?}\nstdout:\n{}\nstderr:\n{}",
            stdout(&out),
            stderr(&out)
        );
        out
    }

    fn json(&self, args: &[&str]) -> Value {
        serde_json::from_slice(&self.ok(args).stdout).unwrap()
    }

    /// The branch names `by ls` lists.
    fn branches(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .json(&["ls", "--json"])
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["name"].as_str().unwrap().to_owned())
            .collect();
        names.sort();
        names
    }

    /// `by map` with the fake agent as the harness.
    fn map(&self, args: &[&str]) -> Output {
        let agent = fake_agent().display().to_string();
        let mut all = vec!["map"];
        all.extend(args);
        all.extend(["--harness", "gemini-cli", "--command", &agent, "--yes"]);
        self.by(&all)
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn lines(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

const SCHEMA: &str = r#"{
  "type": "object",
  "required": ["name", "ok"],
  "additionalProperties": false,
  "properties": {"name": {"type": "string", "minLength": 1}, "ok": {"type": "boolean"}}
}"#;

#[test]
fn items_come_from_every_format_and_fill_the_template() {
    let repo = Repo::new();
    let alpha = repo.file("alpha.json", r#"{"name": "alpha", "ok": true}"#);
    let beta = repo.file(
        "beta.json",
        "```json\n{\"name\": \"beta\", \"ok\": false}\n```",
    );
    let schema = repo.file("schema.json", SCHEMA);
    let template =
        "REPLY_FILE {{item.file}}\nLabel {{item.label}}, #{{index}} of {{map}}, id {{id}}";

    // CSV with a header, explicit ids.
    let csv = repo.file(
        "items.csv",
        &format!("id,file,label\nfirst,{alpha},\"one, quoted\"\nsecond,{beta},two\n"),
    );
    let out = repo.map(&[template, "--items", &csv, "--schema", &schema, "-n", "csv"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("by: map csv: first ok"), "{err}");
    let shown = repo.json(&["show", "csv-second", "--json"]);
    let prompt = shown["prompt"].as_str().unwrap();
    assert!(
        prompt.starts_with(&format!(
            "REPLY_FILE {beta}\nLabel two, #2 of csv, id second"
        )),
        "{prompt}"
    );
    assert!(prompt.contains("## Answer"), "{prompt}");
    let report = repo.json(&["map", "show", "csv", "--json"]);
    assert_eq!(report["done"], 2, "{report}");
    assert_eq!(report["rows"][0]["result"]["name"], "alpha");
    assert_eq!(report["rows"][1]["result"]["ok"], false);
    let first = repo.json(&["show", "csv-first", "--json"]);
    assert!(first["prompt"]
        .as_str()
        .unwrap()
        .contains("Label one, quoted"));

    // JSON lines, ids from the content.
    let jsonl = repo.file(
        "items.jsonl",
        &format!("{{\"file\": \"{alpha}\", \"label\": \"x\"}}\n\n{{\"file\": \"{beta}\", \"label\": \"y\"}}\n"),
    );
    let out = repo.map(&[
        template,
        "--items",
        &jsonl,
        "--schema",
        &schema,
        "-n",
        "lines-of-json",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let report = repo.json(&["map", "show", "lines-of-json", "--json"]);
    assert_eq!(report["done"], 2);
    let id = report["rows"][0]["id"].as_str().unwrap();
    assert_eq!(id.len(), 12, "a content hash: {id}");
    assert!(repo.branches().contains(&format!("lines-of-json-{id}")));

    // A JSON array, on standard input.
    let array = format!("[{{\"id\": 1, \"file\": \"{alpha}\", \"label\": \"z\"}}]");
    let out = repo.by_with_stdin(
        &[
            "map",
            template,
            "--schema",
            &schema,
            "-n",
            "from-stdin",
            "--harness",
            "gemini-cli",
            "--command",
            &fake_agent().display().to_string(),
            "--yes",
        ],
        &array,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(repo.branches().contains(&"from-stdin-1".to_owned()));

    // One item per line, from a command's output; no schema: the result is
    // the reply's text.
    let listing = repo.file(
        "listing.txt",
        &format!("REPLY_FILE {alpha}\nREPLY_FILE {beta}\n"),
    );
    let out = repo.map(&[
        "{{item}}",
        "--from-command",
        &format!("cat {listing}"),
        "-n",
        "plain",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let report = repo.json(&["map", "show", "plain", "--json"]);
    assert_eq!(report["done"], 2, "{report}");
    assert_eq!(
        report["rows"][0]["result"],
        r#"{"name": "alpha", "ok": true}"#
    );

    // The same command again starts nothing: every item is done.
    let before = repo.branches();
    let out = repo.map(&[template, "--items", &csv, "--schema", &schema, "-n", "csv"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!stderr(&out).contains(" ok ("), "{}", stderr(&out));
    assert_eq!(repo.branches(), before);

    // Listed with their progress.
    let maps = repo.json(&["map", "ls", "--json"]);
    assert_eq!(maps.as_array().unwrap().len(), 4, "{maps}");
    let table = stdout(&repo.ok(&["map", "ls"]));
    assert!(table.lines().next().unwrap().starts_with("MAP"), "{table}");
    assert!(table.contains("2 of 2 done"), "{table}");
    let ls = stdout(&repo.ok(&["ls"]));
    assert!(ls.contains("\nmaps\nMAP"), "{ls}");
    let show = stdout(&repo.ok(&["show", "csv"]));
    assert!(show.contains("progress  2 of 2 done"), "{show}");
    assert!(show.contains("second  ok"), "{show}");
}

#[test]
fn an_invalid_answer_gets_one_follow_up_and_an_item_fails_after_its_retries() {
    let repo = Repo::new();
    let schema = repo.file("schema.json", SCHEMA);
    // Missing a required field, then right.
    repo.file("seq/1", r#"{"name": "half"}"#);
    repo.file("seq/2", r#"{"name": "whole", "ok": true}"#);
    let items = repo.file(
        "items.jsonl",
        &format!(
            "{{\"id\": \"fixed\", \"cmd\": \"REPLY_SEQUENCE {}\"}}\n{{\"id\": \"wrong\", \"cmd\": \"say something\"}}\n",
            repo.dir.join("seq").display()
        ),
    );
    let out_file = repo.dir.join("results.jsonl");
    let out = repo.map(&[
        "{{item.cmd}}",
        "--items",
        &items,
        "--schema",
        &schema,
        "--retries",
        "1",
        "--out",
        out_file.to_str().unwrap(),
        "-n",
        "strict",
    ]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("map strict: 1 of 2 done, 1 failed"), "{text}");
    assert!(
        text.contains("by map resume strict --retry-failed"),
        "{text}"
    );

    let rows = lines(&out_file);
    assert_eq!(rows[0]["id"], "fixed");
    assert_eq!(rows[0]["status"], "ok");
    assert_eq!(rows[0]["result"]["name"], "whole");
    assert_eq!(rows[0]["attempts"], 1);
    assert_eq!(rows[1]["id"], "wrong");
    assert_eq!(rows[1]["status"], "failed");
    assert_eq!(rows[1]["attempts"], 2);
    assert_eq!(rows[1]["branch"], "strict-wrong-2");
    // The fake agent echoes the prompt, whose last fenced block is the
    // schema itself: not an answer.
    let error = rows[1]["error"].as_str().unwrap();
    assert!(error.starts_with("after one follow-up turn: "), "{error}");
    assert!(
        error.contains("$: missing the required field \"name\""),
        "{error}"
    );

    // The follow-up named what was wrong and repeated the task.
    let log = repo.json(&["log", "strict-fixed", "--json"]);
    let prompts: Vec<&str> = log
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["activity"] == "prompt")
        .map(|e| e["text"].as_str().unwrap())
        .collect();
    assert_eq!(prompts.len(), 2, "{log}");
    let follow_up = prompts[1];
    assert!(
        follow_up.starts_with("[branchyard map: answer invalid]"),
        "{follow_up}"
    );
    assert!(
        follow_up.contains("- $: missing the required field \"ok\""),
        "{follow_up}"
    );
    assert!(follow_up.ends_with(prompts[0]), "{follow_up}");
    let shown = repo.json(&["show", "strict-fixed", "--json"]);
    assert_eq!(shown["turns"], 2, "{shown}");
    // Each attempt at the failing item had its follow-up too; both kept.
    for name in ["strict-wrong", "strict-wrong-2"] {
        assert_eq!(repo.json(&["show", name, "--json"])["turns"], 2, "{name}");
    }
}

#[test]
fn no_more_branches_run_at_once_than_the_concurrency() {
    let repo = Repo::new();
    let state = repo.dir.join("probe");
    fs::create_dir_all(&state).unwrap();
    fs::write(state.join("started"), "0\n").unwrap();
    fs::write(state.join("active"), "0\n").unwrap();
    // Each item counts itself in, then waits until its pair has started
    // (items 1 and 2 wait for 2 starts, 3 and 4 for 4), so two at a time
    // overlap and a map running one at a time would time out; it records
    // how many ran at once.
    let probe = repo.file(
        "probe.sh",
        &format!(
            r#"S={state}
lock() {{ while ! mkdir "$S/lock" 2>/dev/null; do sleep 0.01; done; }}
lock
k=$(( $(cat "$S/started") + 1 )); echo $k > "$S/started"
a=$(( $(cat "$S/active") + 1 )); echo $a > "$S/active"; echo $a >> "$S/peaks"
rmdir "$S/lock"
want=$(( (k + 1) / 2 * 2 ))
i=0
while [ "$(cat "$S/started")" -lt "$want" ]; do
  i=$((i + 1)); if [ $i -gt 600 ]; then echo timeout >> "$S/peaks"; break; fi
  sleep 0.05
done
lock
a=$(( $(cat "$S/active") - 1 )); echo $a > "$S/active"
rmdir "$S/lock"
printf '```json\n{{"k": %s}}\n```\n' "$k"
"#,
            state = state.display()
        ),
    );
    let schema = repo.file(
        "k.json",
        r#"{"type": "object", "required": ["k"], "properties": {"k": {"type": "integer", "minimum": 1}}}"#,
    );
    let items = repo.file("items.txt", "w\nx\ny\nz\n");
    let out = repo.map(&[
        &format!("SH sh {probe} {{{{item}}}}"),
        "--items",
        &items,
        "--schema",
        &schema,
        "--concurrency",
        "2",
        "-n",
        "pairs",
    ]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    let peaks = fs::read_to_string(state.join("peaks")).unwrap();
    let peaks: Vec<&str> = peaks.lines().collect();
    assert_eq!(peaks.len(), 4, "{peaks:?}");
    assert!(!peaks.contains(&"timeout"), "{peaks:?}");
    let max = peaks
        .iter()
        .map(|p| p.parse::<u32>().unwrap())
        .max()
        .unwrap();
    assert_eq!(max, 2, "{peaks:?}");
    let report = repo.json(&["map", "show", "pairs", "--json"]);
    assert_eq!(report["done"], 4, "{report}");
}

#[test]
fn an_interrupted_map_resumes_without_redoing_what_is_done() {
    let repo = Repo::new();
    let alpha = repo.file("alpha.json", r#"{"name": "alpha", "ok": true}"#);
    let schema = repo.file("schema.json", SCHEMA);
    let items_path = repo.dir.join("items.jsonl");
    fs::write(
        &items_path,
        format!(
            "{{\"id\": \"a\", \"cmd\": \"REPLY_FILE {alpha}\"}}\n{{\"id\": \"b\", \"cmd\": \"HANG\"}}\n"
        ),
    )
    .unwrap();
    let csv = repo.dir.join("out.csv");
    let agent = fake_agent().display().to_string();
    let args = [
        "map",
        "{{item.cmd}}",
        "--items",
        items_path.to_str().unwrap(),
        "--schema",
        &schema,
        "--concurrency",
        "1",
        "--out",
        csv.to_str().unwrap(),
        "-n",
        "halted",
        "--harness",
        "gemini-cli",
        "--command",
        &agent,
        "--yes",
    ];
    let log = fs::File::create(repo.dir.join("first.log")).unwrap();
    let mut first: Child = repo
        .command(BY)
        .args(args)
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    // Wait until item a is recorded and item b's branch is mid-turn.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        assert!(Instant::now() < deadline, "the map never reached item b");
        assert!(first.try_wait().unwrap().is_none(), "the map ended early");
        let recorded = fs::read_to_string(repo.root.join(".branchyard/maps/halted/results.jsonl"))
            .unwrap_or_default();
        let running = repo
            .json(&["ls", "--json"])
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["name"] == "halted-b" && b["status"]["state"] == "running");
        if recorded.contains("\"id\":\"a\"") && running {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let shown = repo.json(&["map", "show", "halted", "--json"]);
    assert_eq!(shown["running"], true, "{shown}");
    assert_eq!(shown["done"], 1);
    first.kill().unwrap();
    first.wait().unwrap();
    // The results so far were written as item a ended.
    let csv_text = fs::read_to_string(&csv).unwrap();
    assert!(csv_text.contains("\na,ok,alpha,true,"), "{csv_text}");
    let a_at = repo.json(&["map", "show", "halted", "--json"])["rows"][0]["at_ms"].clone();

    // Item b would hang again; it answers now.
    fs::write(
        &items_path,
        format!(
            "{{\"id\": \"a\", \"cmd\": \"REPLY_FILE {alpha}\"}}\n{{\"id\": \"b\", \"cmd\": \"REPLY_FILE {alpha}\"}}\n"
        ),
    )
    .unwrap();
    // `by map resume` takes the recorded items, so the edit is not seen.
    let report: Value =
        serde_json::from_slice(&repo.by(&["map", "show", "halted", "--json"]).stdout).unwrap();
    assert_eq!(report["pending"], 1);
    // The same command again reads the edited file.
    let out = repo.by(&args);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("by: map halted: b ok (2 of 2 done)"), "{err}");
    assert!(!err.contains("halted: a ok"), "{err}");
    let report = repo.json(&["map", "show", "halted", "--json"]);
    assert_eq!(report["rows"][0]["at_ms"], a_at, "item a was not run again");
    assert_eq!(report["rows"][1]["branch"], "halted-b-2");
    let branches = repo.branches();
    assert_eq!(
        branches,
        ["halted-a", "halted-b", "halted-b-2"],
        "{branches:?}"
    );
    let interrupted = repo.json(&["show", "halted-b", "--json"]);
    assert_eq!(
        interrupted["status"]["state"], "interrupted",
        "{interrupted}"
    );
    assert_eq!(
        fs::read_to_string(&csv).unwrap().lines().count(),
        3,
        "a header and two rows"
    );
}

#[test]
fn by_map_resume_reruns_the_recorded_command_with_its_items() {
    let repo = Repo::new();
    let answer = repo.dir.join("answer.json");
    fs::write(&answer, "not yet").unwrap();
    let schema = repo.file("schema.json", SCHEMA);
    // The items arrive on standard input, which a resume cannot read again.
    let input = format!("REPLY_FILE {}\n", answer.display());
    let agent = fake_agent().display().to_string();
    let out = repo.by_with_stdin(
        &[
            "map",
            "{{item}}",
            "--schema",
            &schema,
            "-n",
            "again",
            "--retries",
            "0",
            "--harness",
            "gemini-cli",
            "--command",
            &agent,
            "--yes",
        ],
        &input,
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    fs::write(&answer, r#"{"name": "later", "ok": true}"#).unwrap();
    // Failed items stay failed unless asked.
    let out = repo.by(&["map", "resume", "again"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let id = repo.json(&["map", "show", "again", "--json"])["rows"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(repo.branches(), [format!("again-{id}")]);
    let out = repo.by(&["map", "resume", "again", "--retry-failed"]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    let report = repo.json(&["map", "show", "again", "--json"]);
    assert_eq!(report["done"], 1, "{report}");
    assert_eq!(report["rows"][0]["result"]["name"], "later");

    // Forgetting the map keeps its branches.
    let before = repo.branches();
    repo.ok(&["map", "rm", "again"]);
    assert!(repo
        .json(&["map", "ls", "--json"])
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(repo.branches(), before);
    let out = repo.by(&["map", "show", "again"]);
    assert!(
        stderr(&out).contains("no map named \"again\""),
        "{}",
        stderr(&out)
    );
}

#[test]
fn csv_results_a_reduce_and_rm_of_the_done_branches() {
    let repo = Repo::new();
    let alpha = repo.file("alpha.json", r#"{"name": "alpha, a", "ok": true}"#);
    let summary = repo.file("summary.txt", "Two items; one failed.");
    let schema = repo.file("schema.json", SCHEMA);
    let items = repo.file(
        "items.csv",
        &format!("id,cmd\ngood,REPLY_FILE {alpha}\nbad,say nothing useful\n"),
    );
    let out_csv = repo.dir.join("results.csv");
    let reduce_out = repo.dir.join("summary.md");
    let out = repo.map(&[
        "{{item.cmd}}",
        "--items",
        &items,
        "--schema",
        &schema,
        "--retries",
        "0",
        "--out",
        out_csv.to_str().unwrap(),
        "--reduce",
        &format!("REPLY_FILE {summary}\nSummarize them."),
        "--reduce-out",
        reduce_out.to_str().unwrap(),
        "--rm",
        "-n",
        "table",
    ]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("reduce (table-reduce)\nTwo items; one failed."),
        "{text}"
    );
    let csv = fs::read_to_string(&out_csv).unwrap();
    let rows: Vec<&str> = csv.lines().collect();
    assert_eq!(rows[0], "id,status,name,ok,error,branch,attempts,cost_usd");
    assert_eq!(rows[1], "good,ok,\"alpha, a\",true,,table-good,1,");
    assert!(
        rows[2].starts_with("bad,failed,,,\"after one follow-up turn: "),
        "{csv}"
    );
    assert!(rows[2].ends_with(",table-bad,1,"), "{csv}");
    assert_eq!(
        fs::read_to_string(&reduce_out).unwrap(),
        "Two items; one failed.\n"
    );
    // --rm removed the branches whose answers were recorded and the
    // reduce's; the failed item's stays.
    assert_eq!(repo.branches(), ["table-bad"]);
    let report = repo.json(&["map", "show", "table", "--json"]);
    assert_eq!(report["reduce"]["status"], "ok", "{report}");
    assert_eq!(report["reduce"]["branch"], "table-reduce");

    // The reduce was given the results.
    let prompt = branchyard_reduce_prompt(&report);
    assert!(
        prompt.contains("{\"id\":\"good\",\"status\":\"ok\",\"result\":"),
        "{prompt}"
    );

    // Unchanged results are not reduced again.
    let out = repo.map(&[
        "{{item.cmd}}",
        "--items",
        &items,
        "--schema",
        &schema,
        "--reduce",
        &format!("REPLY_FILE {summary}\nSummarize them."),
        "-n",
        "table",
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(!stderr(&out).contains("table-reduce-2"), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("reduce (table-reduce)"),
        "{}",
        stdout(&out)
    );
}

/// The reduce prompt a report's rows make, as the map builds it.
fn branchyard_reduce_prompt(report: &Value) -> String {
    let rows: Vec<branchyard::MapRow> = serde_json::from_value(report["rows"].clone()).unwrap();
    branchyard::map_reduce_prompt("Summarize them.", "table", &rows)
}

#[test]
fn the_total_budget_stops_starting_items() {
    let repo = Repo::new();
    let alpha = repo.file("alpha.json", r#"{"name": "alpha", "ok": true}"#);
    let schema = repo.file("schema.json", SCHEMA);
    let one = repo.file(
        "one.jsonl",
        &format!("{{\"id\": \"a\", \"cmd\": \"REPLY_FILE {alpha}\"}}\n"),
    );
    let out = repo.map(&[
        "{{item.cmd}}",
        "--items",
        &one,
        "--schema",
        &schema,
        "-n",
        "spend",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    // The fake agent reports no cost: record one for item a by hand.
    let results = repo.root.join(".branchyard/maps/spend/results.jsonl");
    let row = fs::read_to_string(&results)
        .unwrap()
        .replace("\"attempts\":1", "\"attempts\":1,\"cost_usd\":1.0");
    fs::write(&results, row).unwrap();
    let two = repo.file(
        "two.jsonl",
        &format!(
            "{{\"id\": \"a\", \"cmd\": \"REPLY_FILE {alpha}\"}}\n{{\"id\": \"b\", \"cmd\": \"REPLY_FILE {alpha}\"}}\n"
        ),
    );
    let out = repo.map(&[
        "{{item.cmd}}",
        "--items",
        &two,
        "--schema",
        &schema,
        "-n",
        "spend",
        "--total-usd",
        "0.5",
    ]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("1 of 2 done, 1 not started ($1.00 reported)"),
        "{text}"
    );
    assert!(text.contains("total budget of $0.50 was reached"), "{text}");
    assert!(!repo.branches().contains(&"spend-b".to_owned()));
    let report = repo.json(&["map", "show", "spend", "--json"]);
    assert_eq!(report["pending"], 1);
}

#[test]
fn a_routed_map_fails_over_from_a_harness_that_exits() {
    let repo = Repo::new();
    let agent = fake_agent().display().to_string();
    fs::write(
        repo.root.join("branchyard.toml"),
        format!(
            r#"
[fleet.default]
candidates = [
  {{ harness = "gemini-cli", command = "/bin/false" }},
  {{ harness = "qwen-code", command = "{agent}" }},
]
exploration = 0
"#
        ),
    )
    .unwrap();
    let alpha = repo.file("alpha.json", r#"{"name": "alpha", "ok": true}"#);
    let schema = repo.file("schema.json", SCHEMA);
    let items = repo.file(
        "items.jsonl",
        &format!("{{\"id\": \"p\", \"cmd\": \"REPLY_FILE {alpha}\"}}\n{{\"id\": \"q\", \"cmd\": \"REPLY_FILE {alpha}\"}}\n"),
    );
    let out = repo.by(&[
        "map",
        "{{item.cmd}}",
        "--items",
        &items,
        "--schema",
        &schema,
        "--auto",
        "--seed",
        "3",
        "-n",
        "routed",
        "--yes",
    ]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    let report = repo.json(&["map", "show", "routed", "--json"]);
    assert_eq!(report["done"], 2, "{report}");
    for row in report["rows"].as_array().unwrap() {
        let branch = row["branch"].as_str().unwrap();
        let info = repo.json(&["show", branch, "--json"]);
        assert_eq!(info["harness"], "qwen-code", "{info}");
    }
}

#[test]
fn mistakes_are_refused_before_any_branch_runs() {
    let repo = Repo::new();
    let items = repo.file("items.jsonl", "{\"id\": \"a\", \"x\": 1}\n");
    let cases: Vec<(Vec<String>, &str)> = vec![
        (
            vec!["{{event.x}}".into(), "--items".into(), items.clone()],
            "{{event.x}} is not a placeholder",
        ),
        (
            vec!["{{item.y}}".into(), "--items".into(), items.clone()],
            "item \"a\" has no {{item.y}}",
        ),
        (
            vec![
                "{{item.x}}".into(),
                "--items".into(),
                items.clone(),
                "--schema".into(),
                repo.file("bad.json", r#"{"type": "string", "pattern": "x"}"#),
            ],
            "\"pattern\" is not supported",
        ),
        (
            vec![
                "{{item.x}}".into(),
                "--items".into(),
                repo.file("empty.jsonl", "\n"),
            ],
            "the map has no items",
        ),
        (
            vec![
                "{{item.x}}".into(),
                "--items".into(),
                repo.file("dup.jsonl", "{\"id\": 1}\n{\"id\": 1}\n"),
            ],
            "give each item a distinct",
        ),
        (
            vec![
                "{{item.x}}".into(),
                "--from-command".into(),
                "exit 3".into(),
            ],
            "exit status: 3",
        ),
    ];
    for (args, expected) in cases {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = repo.map(&args);
        assert!(!out.status.success(), "{args:?}");
        assert!(
            stderr(&out).contains(expected),
            "{expected}\n{}",
            stderr(&out)
        );
    }
    assert!(repo.branches().is_empty());
    // An unknown item format is a usage error.
    let out = repo.map(&["{{item}}", "--input-format", "xml"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    // A map's name with another prompt.
    let alpha = repo.file("alpha.json", r#"{"name": "alpha", "ok": true}"#);
    let one = repo.file("one.txt", &format!("REPLY_FILE {alpha}\n"));
    assert!(repo
        .map(&["{{item}}", "--items", &one, "-n", "named"])
        .status
        .success());
    let out = repo.map(&["{{item}} again", "--items", &one, "-n", "named"]);
    assert!(
        stderr(&out).contains("a map named named exists with a different prompt or schema"),
        "{}",
        stderr(&out)
    );
}

// ---------------------------------------------------------------------------
// Through a server

/// `by serve` on an ephemeral loopback port. Stopped with SIGTERM on drop.
struct Served {
    child: Child,
    url: String,
    token_file: PathBuf,
}

impl Served {
    fn start(repo: &Repo) -> Served {
        let data = repo.dir.join("data");
        let mut serve = repo.command(BY);
        serve.current_dir(&repo.dir).args([
            "serve",
            "--listen",
            "127.0.0.1:0",
            "--quiet",
            "--shutdown-grace",
            "5",
            "--allow-client-commands",
        ]);
        serve.arg("--data-dir").arg(&data);
        serve
            .arg("--repo")
            .arg(format!("app={}", repo.root.display()));
        let log = fs::File::create(repo.dir.join("server.log")).unwrap();
        let mut child = serve
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(log)
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let url = line
            .trim()
            .strip_prefix("listening on ")
            .unwrap_or_else(|| {
                let log = fs::read_to_string(repo.dir.join("server.log")).unwrap_or_default();
                panic!("server did not start: {line:?}\n{log}")
            })
            .to_owned();
        Served {
            child,
            url,
            token_file: data.join("token"),
        }
    }

    /// `by --remote URL --token-file F <args>`, from outside the repository.
    fn by(&self, repo: &Repo, args: &[&str]) -> Output {
        repo.command(BY)
            .current_dir(&repo.dir)
            .arg("--remote")
            .arg(&self.url)
            .arg("--token-file")
            .arg(&self.token_file)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status();
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if self.child.try_wait().unwrap().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn a_map_runs_as_a_server_operation_with_remote() {
    let repo = Repo::new();
    let server = Served::start(&repo);
    let answer = repo.dir.join("answer.json");
    fs::write(&answer, r#"{"name": "alpha", "ok": true}"#).unwrap();
    let bad = repo.dir.join("bad.json");
    fs::write(&bad, "nothing").unwrap();
    let schema = repo.file("schema.json", SCHEMA);
    let items = repo.file(
        "items.csv",
        &format!(
            "id,cmd\none,REPLY_FILE {}\ntwo,REPLY_FILE {}\n",
            answer.display(),
            bad.display()
        ),
    );
    let out_file = repo.dir.join("remote.jsonl");
    let agent = fake_agent().display().to_string();
    let out = server.by(
        &repo,
        &[
            "map",
            "{{item.cmd}}",
            "--items",
            &items,
            "--schema",
            &schema,
            "--retries",
            "0",
            "--out",
            out_file.to_str().unwrap(),
            "-n",
            "far",
            "--harness",
            "gemini-cli",
            "--command",
            &agent,
            "--yes",
            "--reduce",
            &format!("REPLY_FILE {}", answer.display()),
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}\n{}\n{}",
        stdout(&out),
        stderr(&out),
        fs::read_to_string(repo.dir.join("server.log")).unwrap_or_default()
    );
    let err = stderr(&out);
    assert!(err.contains("running on the server as op_"), "{err}");
    // The branches' activity streamed while the operation ran.
    assert!(stdout(&out).contains("far-one │"), "{}", stdout(&out));
    let text = stdout(&out);
    assert!(text.contains("map far: 1 of 2 done, 1 failed"), "{text}");
    assert!(text.contains("reduce (far-reduce)"), "{text}");
    let rows = lines(&out_file);
    assert_eq!(rows[0]["result"]["name"], "alpha");
    assert_eq!(rows[1]["status"], "failed");

    // Recorded on the server, and shown as locally.
    let maps: Value =
        serde_json::from_slice(&server.by(&repo, &["map", "ls", "--json"]).stdout).unwrap();
    assert_eq!(maps[0]["name"], "far", "{maps}");
    assert_eq!(maps[0]["done"], 1);
    let remote_show = stdout(&server.by(&repo, &["map", "show", "far"]));
    let local_show = stdout(&repo.ok(&["map", "show", "far"]));
    assert_eq!(remote_show, local_show);
    let shown = stdout(&server.by(&repo, &["show", "far"]));
    assert!(shown.contains("progress  1 of 2 done, 1 failed"), "{shown}");
    let ls = stdout(&server.by(&repo, &["ls"]));
    assert!(ls.contains("\nmaps\n"), "{ls}");

    // Resumed on the server with the request that started it.
    fs::write(&bad, r#"{"name": "beta", "ok": false}"#).unwrap();
    let out = server.by(&repo, &["map", "resume", "far", "--retry-failed", "--json"]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["done"], 2, "{report}");
    assert_eq!(report["rows"][1]["branch"], "far-two-2");
    assert_eq!(report["reduce"]["branch"], "far-reduce-2");

    // Routing is local only, as for run and fan.
    let out = server.by(&repo, &["map", "{{item}}", "--items", &items, "--auto"]);
    assert!(stderr(&out).contains("local mode only"), "{}", stderr(&out));
    // A map started locally cannot be resumed through the server.
    let one = repo.file("one.txt", &format!("REPLY_FILE {}\n", answer.display()));
    assert!(repo
        .map(&["{{item}}", "--items", &one, "-n", "near"])
        .status
        .success());
    let out = server.by(&repo, &["map", "resume", "near"]);
    assert!(
        stderr(&out).contains("was not started through this server's API"),
        "{}",
        stderr(&out)
    );
    // Forgotten through the server.
    let out = server.by(&repo, &["map", "rm", "far"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let maps: Value =
        serde_json::from_slice(&server.by(&repo, &["map", "ls", "--json"]).stdout).unwrap();
    assert_eq!(maps.as_array().unwrap().len(), 1, "{maps}");
}
