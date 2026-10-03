//! `by task`: tasks and their attempts (docs/task-repos.md). A task in this
//! repository is found here; one with a repository of its own (a folder
//! you granted, or none) under `$BRANCHYARD_HOME/tasks/`. Rewind, fork and
//! accept are the branch operations of an attempt, run in whichever yard
//! holds it. Remotely, `ls` and `show` read the server's tasks.

use std::collections::BTreeMap;
use std::path::Path;

use branchyard::tasks::{self, AttemptView, NewTask, TaskFiles, TaskView};
use branchyard::{TaskOptions, Yard};

use crate::args::{TaskAction, TaskArgs};
use crate::commands::{self, print, Env, Failure, Live, Outcome, Target};
use crate::{json, render};

pub fn main(env: &Env, target: &Target, action: &TaskAction, as_json: bool) -> Outcome {
    if let Target::Remote(remote) = target {
        return remote_main(env, remote, action, as_json);
    }
    match action {
        TaskAction::New(args) => new(
            env,
            &args.prompt,
            args.folder.as_deref(),
            args.no_files,
            args.large_threshold,
            &args.task,
        ),
        TaskAction::Ls => ls(env, as_json),
        TaskAction::Show(args) => {
            let (_, view) = locate(&args.task)?;
            show(env, &view, as_json)
        }
        TaskAction::Open(args) => open(
            &args.task,
            args.attempt.as_deref(),
            args.editor.as_deref(),
            args.print,
        ),
        TaskAction::Rewind(args) => {
            let (yard, view) = locate(&args.task)?;
            let attempt = tasks::pick_attempt(&view, args.attempt.as_deref())?
                .name
                .clone();
            crate::attempts::rewind_in(env, &yard, &attempt, args.to, args.yes, as_json)
        }
        TaskAction::Fork(args) => fork(
            env,
            &args.task,
            &args.prompt,
            args.attempt.as_deref(),
            args.at,
            &args.flags,
        ),
        TaskAction::Accept(args) => accept(
            &args.task,
            args.attempt.as_deref(),
            args.into.as_deref(),
            as_json,
        ),
        TaskAction::Rm(args) => rm(env, &args.task, args.yes, as_json),
    }
}

/// The repository `by` runs in, if any.
fn here() -> Option<Yard> {
    commands::open().ok()
}

/// The yard holding the task `key` names, and the task: this repository's
/// first, then those with a repository of their own.
fn locate(key: &str) -> Result<(Yard, TaskView), Failure> {
    if let Some(yard) = here() {
        if let Ok(view) = tasks::view(&yard, key) {
            return Ok((yard, view));
        }
    }
    if let Some(yard) = tasks::open_home(&tasks::home(), key)? {
        let yard = commands::configure(yard)?;
        let view = tasks::view(&yard, key)?;
        return Ok((yard, view));
    }
    Err(Failure::Message(format!(
        "no task {key}: give its ID (or a prefix of at least 4 characters) or an attempt's \
         name; `by task ls` lists them"
    )))
}

fn new(
    env: &Env,
    prompt: &str,
    folder: Option<&Path>,
    no_files: bool,
    large_threshold: Option<u64>,
    task: &TaskArgs,
) -> Outcome {
    if folder.is_none() && !no_files {
        if large_threshold.is_some() {
            return Err(Failure::Message(
                "--large-threshold applies to a task with a repository of its own: pass \
                 --folder PATH or --no-files"
                    .into(),
            ));
        }
        // A task in this repository is a run: its first attempt is a branch.
        return commands::run(env, &Target::Local, prompt, task);
    }
    let (created, yard) = tasks::create(&NewTask {
        prompt: prompt.to_owned(),
        folder: folder.map(Path::to_path_buf),
        large_threshold,
        ..NewTask::default()
    })?;
    let yard = commands::configure(yard)?;
    eprintln!(
        "by: task {} ({}); its repository is {}",
        created.id,
        match &created.files {
            TaskFiles::Folder { folder } => format!(
                "folder {}, which changes only when you accept an attempt",
                folder.display()
            ),
            other => other.label().to_owned(),
        },
        yard.root().parent().unwrap_or(yard.root()).display()
    );
    commands::run_in(env, &yard, prompt, task, Some(&created.id))
}

fn ls(env: &Env, as_json: bool) -> Outcome {
    let mut views = match here() {
        Some(yard) => tasks::list(&yard)?,
        None => Vec::new(),
    };
    views.extend(tasks::home_tasks(&tasks::home())?);
    print_list(env, &views, as_json)
}

fn print_list(env: &Env, views: &[TaskView], as_json: bool) -> Outcome {
    if as_json {
        return print(&json::text(
            &serde_json::to_value(views).unwrap_or_default(),
        ));
    }
    if views.is_empty() {
        return print("no tasks; start one with: by task new \"<prompt>\" (or by run)\n");
    }
    print(&table(views, env.style()))
}

/// One line per task: its ID, what its files are, its attempts and its
/// title.
pub fn table(views: &[TaskView], style: render::Style) -> String {
    let rows: Vec<[String; 4]> = views
        .iter()
        .map(|view| {
            [
                view.task.id.clone(),
                view.task.files.label().to_owned(),
                attempts_summary(&view.attempts),
                view.task.title.clone(),
            ]
        })
        .collect();
    let width = |i: usize| {
        rows.iter()
            .map(|r| r[i].chars().count())
            .max()
            .unwrap_or(0)
            .max(["TASK", "FILES", "ATTEMPTS", "TITLE"][i].len())
    };
    let (w0, w1, w2) = (width(0), width(1), width(2));
    let mut out = style.paint(
        render::Tone::Bold,
        &format!("{:w0$}  {:w1$}  {:w2$}  TITLE", "TASK", "FILES", "ATTEMPTS"),
    );
    out.push('\n');
    for [id, files, attempts, title] in rows {
        out.push_str(&format!("{id:w0$}  {files:w1$}  {attempts:w2$}  {title}\n"));
    }
    out
}

/// `2 (1 ready, 1 merged)`.
fn attempts_summary(attempts: &[AttemptView]) -> String {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for attempt in attempts {
        let label = match &attempt.status {
            Some(status) => short_status(status),
            None => "removed".into(),
        };
        *counts.entry(label).or_default() += 1;
    }
    let parts: Vec<String> = counts
        .into_iter()
        .map(|(label, n)| format!("{n} {label}"))
        .collect();
    format!("{} ({})", attempts.len(), parts.join(", "))
}

fn short_status(status: &branchyard::BranchStatus) -> String {
    let (text, _) = render::status_text(status);
    // "failed: why" and "merged into main" read as their first word.
    text.split([':', ' ']).next().unwrap_or_default().to_owned()
}

fn show(env: &Env, view: &TaskView, as_json: bool) -> Outcome {
    if as_json {
        return print(&json::text(&serde_json::to_value(view).unwrap_or_default()));
    }
    print(&details(view, env.style()))
}

/// `by task show`'s text.
pub fn details(view: &TaskView, style: render::Style) -> String {
    let task = &view.task;
    let mut out = format!("{}\n", style.paint(render::Tone::Bold, &task.title));
    let mut field = |name: &str, value: String| {
        out.push_str(&format!("  {name:<10} {value}\n"));
    };
    field("task", task.id.clone());
    field(
        "files",
        match &task.files {
            TaskFiles::Folder { folder } => format!("folder {}", folder.display()),
            other => other.label().to_owned(),
        },
    );
    field("by", task.by.clone());
    field("created", tasks::utc(task.created_ms));
    field("started by", task.origin.clone());
    if !task.policy.is_empty() {
        field("policy", task.policy.clone());
    }
    field("repository", view.repository.display().to_string());
    if let Some(accepted) = &view.accepted {
        field("accepted", short(accepted).to_owned());
    }
    if let Some(at) = &view.folder_at {
        field("folder at", short(at).to_owned());
    }
    if task.asked.trim() != task.title {
        out.push_str("  asked:\n");
        for line in task.asked.lines() {
            out.push_str(&format!("    {line}\n"));
        }
    }
    out.push_str(&format!("  attempts ({}):\n", view.attempts.len()));
    for attempt in &view.attempts {
        let status = match &attempt.status {
            Some(status) => render::status_text(status).0,
            None => "removed".into(),
        };
        let mut line = format!("    {}  {status}", attempt.name);
        if let Some(harness) = &attempt.harness {
            line.push_str(&format!("  {harness}"));
        }
        if attempt.status.is_some() {
            line.push_str(&format!(
                "  {} turn{}",
                attempt.turns,
                if attempt.turns == 1 { "" } else { "s" }
            ));
        }
        if let Some(checkpoint) = attempt.checkpoint {
            line.push_str(&format!("  at checkpoint {checkpoint}"));
        }
        if let Some(parent) = &attempt.forked_from {
            line.push_str(&format!("  forked from {parent}"));
        }
        out.push_str(&line);
        out.push('\n');
        if !attempt.conversation.is_empty() {
            out.push_str(&format!(
                "      conversation: {}\n",
                attempt.conversation.join(", ")
            ));
        }
    }
    out
}

fn short(commit: &str) -> &str {
    commit.get(..10).unwrap_or(commit)
}

/// For `by watch`: each attempt's task, as one line.
pub fn watch_lines(views: &[TaskView]) -> BTreeMap<String, String> {
    let mut lines = BTreeMap::new();
    for view in views {
        let total = view.attempts.len();
        for (i, attempt) in view.attempts.iter().enumerate() {
            lines.insert(
                attempt.name.clone(),
                format!(
                    "{} · attempt {} of {total} · {}",
                    view.task.id,
                    i + 1,
                    view.task.title
                ),
            );
        }
    }
    lines
}

/// `by show`'s line for a branch that is a task's attempt, and its JSON.
pub fn show_line(view: &TaskView, branch: &str) -> (String, serde_json::Value) {
    let total = view.attempts.len();
    let index = view
        .attempts
        .iter()
        .position(|a| a.name == branch)
        .map_or(0, |i| i + 1);
    let text = format!(
        "{} (attempt {index} of {total}): {}",
        view.task.id, view.task.title
    );
    let value = serde_json::json!({
        "id": view.task.id,
        "title": view.task.title,
        "attempt": index,
        "attempts": total,
    });
    (text, value)
}

fn open(key: &str, attempt: Option<&str>, editor: Option<&str>, print_only: bool) -> Outcome {
    let (_, view) = locate(key)?;
    let attempt = tasks::pick_attempt(&view, attempt)?;
    let worktree = attempt
        .worktree
        .clone()
        .filter(|w| w.is_dir())
        .ok_or_else(|| Failure::Message(format!("{} has no worktree", attempt.name)))?;
    if print_only {
        return print(&format!("{}\n", worktree.display()));
    }
    let plan = crate::open::plan(&worktree, editor, &|name| std::env::var(name).ok())?;
    eprintln!("by: opening {} with {}", worktree.display(), plan.argv[0]);
    crate::open::launch(&plan)
}

fn fork(
    env: &Env,
    key: &str,
    prompt: &str,
    attempt: Option<&str>,
    at: Option<u32>,
    flags: &TaskArgs,
) -> Outcome {
    let (yard, view) = locate(key)?;
    let attempt = tasks::pick_attempt(&view, attempt)?.name.clone();
    let branch = yard.branch(&attempt)?;
    let live = Live::start(env, flags, flags.delegate.is_some(), branch.provider()?);
    let options = TaskOptions {
        workspace: commands::workspace(env, &yard)?,
        join_task: Some(view.task.id.clone()),
        ..live.options(flags)?
    };
    let result = match at {
        Some(turn) => branch.fork_at(turn, prompt, options),
        None => branch.fork(prompt, true, options),
    };
    if let (Ok(forked), Some(_)) = (&result, at) {
        crate::attempts::announce_fork(forked);
    }
    live.finish(env, result)
}

fn accept(key: &str, attempt: Option<&str>, into: Option<&str>, as_json: bool) -> Outcome {
    let (yard, view) = locate(key)?;
    let accepted = tasks::accept(&yard, &view.task.id, attempt, into)?;
    if as_json {
        return print(&json::text(
            &serde_json::to_value(&accepted).unwrap_or_default(),
        ));
    }
    let mut text = format!(
        "accepted {} into {} ({}..{})\n",
        accepted.attempt,
        accepted.target,
        short(&accepted.previous),
        short(&accepted.commit)
    );
    if let Some(folder) = &accepted.folder {
        text.push_str(&format!(
            "{}: {} written, {} removed\n",
            folder.display(),
            accepted.written.len(),
            accepted.removed.len()
        ));
        for path in &accepted.written {
            text.push_str(&format!("  wrote   {path}\n"));
        }
        for path in &accepted.removed {
            text.push_str(&format!("  removed {path}\n"));
        }
    }
    print(&text)
}

fn rm(env: &Env, key: &str, yes: bool, as_json: bool) -> Outcome {
    let (yard, view) = locate(key)?;
    if !yes {
        let question = format!(
            "Remove task {} and its {} attempt(s)?{}",
            view.task.id,
            view.attempts.iter().filter(|a| a.status.is_some()).count(),
            match &view.task.files {
                TaskFiles::Folder { folder } => {
                    format!(" The folder {} is not touched.", folder.display())
                }
                _ => String::new(),
            }
        );
        if !crate::attempts::confirm(env, &question) {
            return Err(Failure::Message(format!(
                "removing task {} removes its attempts; pass --yes to confirm (or run on a \
                 terminal)",
                view.task.id
            )));
        }
    }
    let removed = match view.task.files {
        TaskFiles::Repository => tasks::remove(&yard, &view.task.id)?,
        _ => {
            drop(yard);
            tasks::remove_home(&tasks::home(), &view.task.id)?
        }
    };
    if as_json {
        return print(&json::text(
            &serde_json::to_value(&removed).unwrap_or_default(),
        ));
    }
    print(&format!("removed task {}\n", removed.task.id))
}

fn remote_main(
    env: &Env,
    remote: &crate::remote::Remote,
    action: &TaskAction,
    as_json: bool,
) -> Outcome {
    match action {
        TaskAction::Ls => {
            let views = remote.repo.tasks()?;
            print_list(env, &views, as_json)
        }
        TaskAction::Show(args) => {
            let view = remote.repo.task(&args.task)?;
            show(env, &view, as_json)
        }
        _ => Err(Failure::Message(
            "by --remote task lists and shows a server's tasks; start one with by --remote run, \
             accept an attempt with by --remote merge, and run the other task commands on the \
             server's host"
                .into(),
        )),
    }
}
