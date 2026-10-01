//! `by map`: one prompt over every item of a list, each on its own branch,
//! the answers collected into a table; `by map resume|ls|show|rm`. The map
//! itself runs in the SDK ([`branchyard::Yard::map`]) locally, or as a
//! server operation with `--remote` ([`crate::remote::map`]). See
//! docs/map.md.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use branchyard::{
    ItemFormat, MapItem, MapOptions, MapProgress, MapReport, MapSpec, MapStatus, MapSummary,
    TaskOptions, Yard,
};
use serde_json::{json, Value};

use crate::args::{MapAction, MapArgs, TaskArgs};
use crate::commands::{self, print, Env, Failure, Outcome, Target};
use crate::render::{self, Cell, Column, Style, Tone};

pub fn main(
    env: &Env,
    target: &Target,
    action: Option<&MapAction>,
    args: &MapArgs,
    json: bool,
) -> Outcome {
    match action {
        None => match target {
            Target::Local => run_local(env, args, &args.task, None, args.retry_failed, json),
            Target::Remote(remote) => crate::remote::map(env, remote, args, json),
        },
        Some(MapAction::Resume { name, retry_failed }) => match target {
            Target::Local => resume(env, name, *retry_failed, json),
            Target::Remote(remote) => {
                crate::remote::map_resume(env, remote, name, *retry_failed, json)
            }
        },
        Some(MapAction::Ls) => {
            let maps = match target {
                Target::Local => commands::open()?.maps()?,
                Target::Remote(remote) => remote.repo.maps()?,
            };
            match json {
                true => print(&crate::json::text(
                    &serde_json::to_value(&maps).unwrap_or_default(),
                )),
                false if maps.is_empty() => {
                    print("no maps; start one with: by map \"<prompt>\" --items FILE\n")
                }
                false => print(&maps_table(&maps, env.style())),
            }
        }
        Some(MapAction::Show { name }) => {
            let report = match target {
                Target::Local => commands::open()?.map_report(name)?,
                Target::Remote(remote) => remote.repo.map(name)?,
            };
            show_report(env, &report, json)
        }
        Some(MapAction::Rm { name }) => {
            match target {
                Target::Local => commands::open()?.remove_map(name)?,
                Target::Remote(remote) => remote.repo.remove_map(name)?,
            }
            print(&format!(
                "forgot map {name}; its branches stay (by ls lists them, by rm removes one)\n"
            ))
        }
    }
}

/// The items, from `--items`, `--from-command` or standard input. A
/// command runs in `dir` with `sh -c`, as you; never inside a harness's
/// branch, where nothing can be trusted to run.
pub fn read_items(args: &MapArgs, dir: &Path) -> Result<Vec<MapItem>, Failure> {
    let stdin = || -> Result<String, Failure> {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        Ok(text)
    };
    let (text, hint) = match (&args.items, &args.from_command) {
        (Some(path), _) if path == "-" => (stdin()?, None),
        (Some(path), _) => (
            std::fs::read_to_string(path)
                .map_err(|e| Failure::Message(format!("--items {path}: {e}")))?,
            ItemFormat::for_path(path),
        ),
        (None, Some(command)) => (from_command(command, dir)?, None),
        (None, None) if !std::io::IsTerminal::is_terminal(&std::io::stdin()) => (stdin()?, None),
        (None, None) => {
            return Err(Failure::Message(
                "give the map its items with --items FILE, --from-command CMD, or on standard \
                 input"
                    .into(),
            ))
        }
    };
    let format = args
        .input_format
        .or(hint)
        .unwrap_or_else(|| ItemFormat::detect(&text));
    let items = branchyard::parse_map_items(&text, format)?;
    if items.is_empty() {
        return Err(Failure::Message(format!(
            "the map has no items (read as {format}); nothing to run"
        )));
    }
    Ok(items)
}

fn from_command(command: &str, dir: &Path) -> Result<String, Failure> {
    if std::env::var_os(branchyard::ENV_BRANCH).is_some_and(|v| !v.is_empty()) {
        return Err(Failure::Message(
            "inside a harness, by map does not run --from-command; pass the items with --items"
                .into(),
        ));
    }
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| Failure::Message(format!("--from-command: could not run sh: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = stderr.trim_end().lines().rev().take(5).collect();
        return Err(Failure::Message(format!(
            "--from-command {command:?} {}: {}",
            out.status,
            tail.into_iter().rev().collect::<Vec<_>>().join(" / ")
        )));
    }
    String::from_utf8(out.stdout)
        .map_err(|_| Failure::Message("--from-command printed text that is not UTF-8".into()))
}

/// `--schema`'s JSON.
pub fn read_schema(args: &MapArgs) -> Result<Option<Value>, Failure> {
    let Some(path) = &args.schema else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(path)
        .map_err(|e| Failure::Message(format!("--schema {path}: {e}")))?;
    let schema: Value = serde_json::from_str(&text)
        .map_err(|e| Failure::Message(format!("--schema {path} is not JSON: {e}")))?;
    branchyard::JsonSchema::new(schema.clone())
        .map_err(|e| Failure::Message(format!("--schema {path}: {e}")))?;
    Ok(Some(schema))
}

/// The map's spec from the command line; its items from `stored` when
/// resuming, else read now.
pub fn spec_of(
    args: &MapArgs,
    task: &TaskArgs,
    dir: &Path,
    stored: Option<&MapSpec>,
) -> Result<MapSpec, Failure> {
    let prompt = args.prompt.clone().unwrap_or_default();
    let name = task
        .name
        .clone()
        .unwrap_or_else(|| branchyard::map_default_name(&prompt));
    let items = match stored {
        Some(spec) => spec.items.clone(),
        None => read_items(args, dir)?,
    };
    let mut spec = MapSpec::new(name, prompt, items);
    spec.schema = read_schema(args)?;
    spec.concurrency = args
        .concurrency
        .unwrap_or(branchyard::MAP_DEFAULT_CONCURRENCY);
    spec.retries = args.retries.unwrap_or(branchyard::MAP_DEFAULT_RETRIES);
    spec.total_usd = args.total_usd;
    spec.reduce = args.reduce.clone();
    spec.remove_done = args.rm;
    Ok(spec)
}

/// Write `text` to `path` through a temporary file beside it and a rename,
/// so a reader never sees half of it.
pub fn write_atomic(path: &Path, text: &str) -> Result<(), Failure> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let file = path
        .file_name()
        .ok_or_else(|| Failure::Message(format!("{} is not a file name", path.display())))?;
    let temporary = dir.join(format!(
        ".{}.tmp.{}",
        file.to_string_lossy(),
        std::process::id()
    ));
    std::fs::write(&temporary, text)
        .and_then(|()| std::fs::rename(&temporary, path))
        .map_err(|e| Failure::Message(format!("write {}: {e}", path.display())))
}

/// The results file's text: CSV for a `.csv` path, else JSON lines.
pub fn results_text(path: &Path, report: &MapReport) -> String {
    let csv = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("csv"));
    match csv {
        true => branchyard::map_rows_csv(&report.rows, &report.columns),
        false => branchyard::map_rows_jsonl(&report.rows),
    }
}

/// The line said as each item ends.
pub fn progress_line(map: &str, progress: &MapProgress) -> String {
    let row = &progress.row;
    let what = match row.status {
        MapStatus::Ok => "ok".to_owned(),
        MapStatus::Failed => format!(
            "failed: {}",
            render::truncate(row.error.as_deref().unwrap_or_default(), 160)
        ),
    };
    let mut counts = format!("{} of {} done", progress.done, progress.total);
    if progress.failed > 0 {
        counts.push_str(&format!(", {} failed", progress.failed));
    }
    format!("by: map {map}: {} {what} ({counts})", row.id)
}

fn run_local(
    env: &Env,
    args: &MapArgs,
    task: &TaskArgs,
    stored: Option<MapSpec>,
    retry_failed: bool,
    json: bool,
) -> Outcome {
    let yard = commands::open()?;
    let mut spec = spec_of(args, task, yard.root(), stored.as_ref())?;
    spec.launch = match stored {
        Some(stored) => stored.launch,
        None => launch(),
    };
    let routed = crate::fleet_cmd::is_routed(task);
    if !routed {
        // A login near its 5-hour or weekly limit (docs/usage.md).
        let harness = task.harness.clone().unwrap_or_else(|| "claude-code".into());
        crate::usage::guard(&[harness])?;
    }
    let workspace = commands::workspace(env, &yard)?;
    let live = commands::Live::start_to(env, task, true, json, None);
    let options = TaskOptions {
        workspace,
        ..live.options(task)?
    };
    let mut map = MapOptions {
        task: options,
        kind: task.kind,
        retry_failed,
        ..MapOptions::default()
    };
    if routed {
        let fleet = crate::fleet_cmd::table(task)?;
        let mut how = crate::fleet_cmd::route_options(task, None);
        how.excluded = crate::usage::route_exclusions(fleet);
        map.fleet = Some(fleet.clone());
        map.route = how;
    }
    let out = args.out.as_ref().map(PathBuf::from);
    let name = spec.name.clone();
    let writer = Arc::new(Mutex::new(()));
    let write_out = {
        let (yard, out, name, writer) = (yard.clone(), out.clone(), name.clone(), writer.clone());
        move || -> Result<(), Failure> {
            let Some(out) = &out else { return Ok(()) };
            let _held = writer.lock().unwrap_or_else(|e| e.into_inner());
            let report = yard.map_report(&name)?;
            write_atomic(out, &results_text(out, &report))
        }
    };
    map.progress = Some({
        let write_out = write_out.clone();
        let name = name.clone();
        Arc::new(move |progress: &MapProgress| {
            eprintln!("{}", progress_line(&name, progress));
            if let Err(error) = write_out() {
                eprintln!("by: map {name}: {error}");
            }
        })
    });
    // What an earlier run of this map already recorded.
    let earlier = yard
        .map_report(&name)
        .ok()
        .filter(|r| r.done + r.failed > 0)
        .map(|r| format!(" ({} done and {} failed before)", r.done, r.failed))
        .unwrap_or_default();
    eprintln!(
        "by: map {name}: {} item(s){earlier}, {} at once, {} retr{} each",
        spec.items.len(),
        spec.concurrency,
        spec.retries,
        if spec.retries == 1 { "y" } else { "ies" }
    );
    let result = yard.map(spec, &map);
    live.console.finish();
    let report = result?;
    write_out()?;
    if let (Some(path), Some(text)) = (
        &args.reduce_out,
        report.reduce.as_ref().and_then(|r| r.text.as_ref()),
    ) {
        write_atomic(Path::new(path), &format!("{}\n", text.trim_end()))?;
    }
    finish(env, &report, out.as_deref(), json)
}

/// What `by map resume` needs: this command line and where it ran.
fn launch() -> Value {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let cwd = std::env::current_dir().unwrap_or_default();
    json!({ "argv": argv, "cwd": cwd })
}

/// Run `name` again with the command line that started it, from where it
/// ran, with its recorded items.
fn resume(env: &Env, name: &str, retry_failed: bool, json: bool) -> Outcome {
    let yard = commands::open()?;
    let stored = yard.map_spec(name)?;
    let argv: Vec<String> = stored.launch["argv"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let cwd = stored.launch["cwd"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| yard.root().to_path_buf());
    let cli = crate::args::parse_from(std::iter::once("by".to_owned()).chain(argv))
        .map_err(|e| Failure::Message(format!("map {name}'s recorded command line: {e}")))?;
    let Some(command) = cli.command else {
        return Err(Failure::Message(format!(
            "map {name} has no recorded command line to resume; run it again with by map"
        )));
    };
    std::env::set_current_dir(&cwd)
        .map_err(|e| Failure::Message(format!("map {name} ran in {}: {e}", cwd.display())))?;
    let vars = |var: &str| std::env::var(var).ok();
    let (_, command) =
        crate::defaults::apply(&cwd, &vars, cli.globals, command).map_err(Failure::Message)?;
    let crate::args::Command::Map {
        action: None, map, ..
    } = command
    else {
        return Err(Failure::Message(format!(
            "map {name}'s recorded command line is not a by map run"
        )));
    };
    run_local(
        env,
        &map,
        &map.task,
        Some(stored),
        retry_failed || map.retry_failed,
        json,
    )
}

/// The closing summary, and the exit status: failure when an item failed,
/// the total budget left items pending, or the reduce failed.
pub fn finish(env: &Env, report: &MapReport, out: Option<&Path>, json: bool) -> Outcome {
    if json {
        print(&crate::json::text(
            &serde_json::to_value(report).unwrap_or_default(),
        ))?;
    } else {
        let style = env.style();
        let mut text = format!(
            "\nmap {}: {} of {} done",
            report.name, report.done, report.total
        );
        if report.failed > 0 {
            text.push_str(&format!(", {} failed", report.failed));
        }
        if report.pending > 0 {
            text.push_str(&format!(", {} not started", report.pending));
        }
        if report.spent_usd > 0.0 {
            text.push_str(&format!(" ({} reported)", render::usd(report.spent_usd)));
        }
        text.push('\n');
        let failed: Vec<_> = report
            .rows
            .iter()
            .filter(|r| r.status == MapStatus::Failed)
            .collect();
        if !failed.is_empty() {
            text.push_str(&format!("\n{}\n", style.paint(Tone::Red, "failed")));
            for row in failed {
                text.push_str(&format!(
                    "  {}  {}  {}\n",
                    row.id,
                    row.branch.as_deref().unwrap_or("-"),
                    render::truncate(row.error.as_deref().unwrap_or_default(), 200)
                ));
            }
        }
        if let Some(stopped) = &report.stopped {
            text.push_str(&format!("\n{stopped}\n"));
        }
        if let Some(reduce) = &report.reduce {
            text.push_str(&reduce_text(reduce, style));
        }
        text.push_str(&format!("\n{}\n", style.paint(Tone::Dim, "next")));
        if let Some(out) = out {
            text.push_str(&format!("  results in {}\n", out.display()));
        }
        text.push_str(&format!("  by map show {}\n", report.name));
        if report.failed > 0 || report.pending > 0 {
            text.push_str(&format!(
                "  by map resume {}{}\n",
                report.name,
                if report.failed > 0 {
                    " --retry-failed"
                } else {
                    ""
                }
            ));
        }
        print(&text)?;
    }
    let reduce_failed = report
        .reduce
        .as_ref()
        .is_some_and(|r| r.status == MapStatus::Failed);
    match report.failed > 0 || report.stopped.is_some() || reduce_failed {
        true => Err(Failure::Reported),
        false => Ok(()),
    }
}

fn reduce_text(reduce: &branchyard::MapReduce, style: Style) -> String {
    let branch = reduce.branch.as_deref().unwrap_or("-");
    match (&reduce.text, &reduce.error) {
        (Some(answer), _) => format!(
            "\n{} ({branch})\n{}\n",
            style.paint(Tone::Dim, "reduce"),
            answer.trim_end()
        ),
        (None, error) => format!(
            "\n{} ({branch}): {}\n",
            style.paint(Tone::Red, "reduce failed"),
            error.as_deref().unwrap_or("no answer")
        ),
    }
}

fn maps_table(maps: &[MapSummary], style: Style) -> String {
    let columns = [
        Column {
            header: "MAP",
            max: 40,
            right: false,
        },
        Column {
            header: "PROGRESS",
            max: 60,
            right: false,
        },
        Column {
            header: "COST",
            max: 12,
            right: true,
        },
        Column {
            header: "PROMPT",
            max: 60,
            right: false,
        },
    ];
    let rows: Vec<Vec<Cell>> = maps
        .iter()
        .map(|m| {
            let tone = match (m.running, m.failed, m.pending) {
                (true, _, _) => Tone::Cyan,
                (false, 0, 0) => Tone::Green,
                (false, 0, _) => Tone::Yellow,
                _ => Tone::Red,
            };
            vec![
                Cell::plain(m.name.clone()),
                Cell::toned(m.progress(), tone),
                Cell::plain(match m.spent_usd > 0.0 {
                    true => render::usd(m.spent_usd),
                    false => "-".into(),
                }),
                Cell::plain(m.prompt.replace('\n', " ")),
            ]
        })
        .collect();
    render::table(&columns, &rows, style)
}

/// `by map show`.
pub fn show_report(env: &Env, report: &MapReport, json: bool) -> Outcome {
    if json {
        return print(&crate::json::text(
            &serde_json::to_value(report).unwrap_or_default(),
        ));
    }
    let style = env.style();
    let summary = MapSummary {
        name: report.name.clone(),
        prompt: report.prompt.clone(),
        total: report.total,
        done: report.done,
        failed: report.failed,
        pending: report.pending,
        running: report.running,
        spent_usd: report.spent_usd,
        created_ms: 0,
        updated_ms: 0,
    };
    let mut text = render::key_values(
        &[
            ("map", report.name.clone()),
            ("progress", summary.progress()),
            (
                "cost",
                render::cost_text((report.spent_usd > 0.0).then_some(report.spent_usd)),
            ),
            (
                "prompt",
                render::truncate(&report.prompt.replace('\n', " "), 200),
            ),
        ],
        style,
    );
    let columns = [
        Column {
            header: "ITEM",
            max: 30,
            right: false,
        },
        Column {
            header: "STATUS",
            max: 8,
            right: false,
        },
        Column {
            header: "BRANCH",
            max: 50,
            right: false,
        },
        Column {
            header: "TRIES",
            max: 5,
            right: true,
        },
        Column {
            header: "COST",
            max: 10,
            right: true,
        },
        Column {
            header: "RESULT",
            max: 80,
            right: false,
        },
    ];
    let rows: Vec<Vec<Cell>> = report
        .rows
        .iter()
        .map(|row| {
            let (status, tone, detail) = match row.status {
                MapStatus::Ok => (
                    "ok",
                    Tone::Green,
                    row.result
                        .as_ref()
                        .map(|r| match r {
                            Value::String(text) => text.replace('\n', " "),
                            other => other.to_string(),
                        })
                        .unwrap_or_default(),
                ),
                MapStatus::Failed => ("failed", Tone::Red, row.error.clone().unwrap_or_default()),
            };
            vec![
                Cell::plain(row.id.clone()),
                Cell::toned(status, tone),
                Cell::plain(row.branch.clone().unwrap_or_else(|| "-".into())),
                Cell::plain(row.attempts.to_string()),
                Cell::plain(render::cost_text(row.cost_usd)),
                Cell::plain(detail),
            ]
        })
        .collect();
    if !rows.is_empty() {
        text.push('\n');
        text.push_str(&render::table(&columns, &rows, style));
    }
    if let Some(reduce) = &report.reduce {
        text.push_str(&reduce_text(reduce, style));
    }
    print(&text)
}

/// The maps section `by ls` adds after the branches, when there are maps.
pub fn ls_section(target: &Target, style: Style) -> Option<String> {
    let maps = match target {
        Target::Local => commands::open().ok()?.maps().ok()?,
        // A server without maps answers 404: nothing to add.
        Target::Remote(remote) => remote.repo.maps().ok()?,
    };
    (!maps.is_empty()).then(|| format!("\nmaps\n{}", maps_table(&maps, style)))
}

/// `by watch`'s line for the maps running or unfinished, locally.
pub fn watch_line(yard: &Yard) -> Option<String> {
    let maps = yard.maps().ok()?;
    let parts: Vec<String> = maps
        .iter()
        .filter(|m| m.running || m.pending > 0)
        .map(|m| format!("{} {}", m.name, m.progress()))
        .collect();
    (!parts.is_empty()).then(|| parts.join("; "))
}

/// `by show NAME` for a name that is a map, not a branch.
pub fn show_if_map(env: &Env, target: &Target, name: &str, json: bool) -> Option<Outcome> {
    let report = match target {
        Target::Local => commands::open().ok()?.map_report(name).ok()?,
        Target::Remote(remote) => remote.repo.map(name).ok()?,
    };
    Some(show_report(env, &report, json))
}
