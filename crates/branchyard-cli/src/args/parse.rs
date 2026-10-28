//! Parsing the command line on a large stack, the hints added to clap's
//! errors, the server-call form, and the value parsers flags use.

use super::*;

/// The stack [`parse_from`] parses on: clap's derived parser for every
/// command is one function whose unoptimized frame outgrew the 2 MiB a
/// spawned thread (a test, a worker) gets, so it never depends on the
/// caller's stack.
pub(crate) const PARSE_STACK: usize = 8 << 20;

/// Parse `by`'s command line, program name first.
pub fn parse_from<I, T>(argv: I) -> Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let argv: Vec<OsString> = argv.into_iter().map(Into::into).collect();
    on_parse_stack(move || parse_argv(argv))
}

/// Run `f` on a thread with [`PARSE_STACK`], passing on its panic.
#[allow(clippy::expect_used)] // ratchet: branchyard-cli
pub(crate) fn on_parse_stack<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    std::thread::Builder::new()
        .name("by-args".into())
        .stack_size(PARSE_STACK)
        .spawn(f)
        .expect("could not start the argument parser's thread")
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

#[allow(clippy::expect_used)] // ratchet: branchyard-cli
pub(crate) fn parse_argv(argv: Vec<OsString>) -> Result<Cli, clap::Error> {
    let mut cmd = build_command();
    let matches = cmd
        .try_get_matches_from_mut(argv.iter().cloned())
        .map_err(|error| with_step_tip(with_prompt_tip(error, &cmd, &argv)))?;
    Cli::from_arg_matches(&matches).map_err(|error| {
        // Checked flags fail here, after clap's own checks; format the
        // error with the command whose usage explains it.
        let mut sub = &mut cmd;
        let mut m = &matches;
        while let Some((name, next)) = m.subcommand() {
            sub = sub
                .find_subcommand_mut(name)
                .expect("matched subcommands exist");
            m = next;
        }
        error.format(sub)
    })
}

/// A stray word after a prompt is most likely an unquoted prompt.
pub(crate) fn with_prompt_tip(
    mut error: clap::Error,
    cmd: &clap::Command,
    argv: &[OsString],
) -> clap::Error {
    if error.kind() != ErrorKind::UnknownArgument {
        return error;
    }
    match error.get(ContextKind::InvalidArg) {
        Some(ContextValue::String(extra)) if !extra.starts_with('-') => {}
        _ => return error,
    }
    let takes_prompt = argv.iter().skip(1).find_map(|arg| {
        let sub = cmd.find_subcommand(arg.to_str()?)?;
        Some(
            sub.get_positionals()
                .any(|p| matches!(p.get_id().as_str(), "prompt" | "text")),
        )
    });
    if takes_prompt == Some(true) {
        error.insert(
            ContextKind::Suggested,
            ContextValue::StyledStrs(vec!["quote a prompt that contains spaces".into()]),
        );
    }
    error
}

/// `by init`'s protocol steps are one per call: say so beside clap's
/// conflict, and name the topics when a step lacks one.
pub(crate) fn with_step_tip(mut error: clap::Error) -> clap::Error {
    let steps = ["--next", "--dry-run", "--apply"];
    let names = |kind| match error.get(kind) {
        Some(ContextValue::String(arg)) => vec![arg.clone()],
        Some(ContextValue::Strings(args)) => args.clone(),
        _ => Vec::new(),
    };
    let is_step = |arg: &String| steps.iter().any(|s| arg.starts_with(s));
    let tip = match error.kind() {
        ErrorKind::ArgumentConflict
            if names(ContextKind::InvalidArg).iter().any(is_step)
                && names(ContextKind::PriorArg).iter().any(is_step) =>
        {
            "--next, --dry-run and --apply are separate steps; give one".to_owned()
        }
        ErrorKind::MissingRequiredArgument
            if names(ContextKind::InvalidArg)
                .iter()
                .any(|a| a == "<TOPIC>") =>
        {
            let ids: Vec<&str> = branchyard_setup::Topic::ALL
                .iter()
                .map(|t| t.id())
                .collect();
            format!("give a topic: {}", ids.join(", "))
        }
        _ => return error,
    };
    error.insert(
        ContextKind::Suggested,
        ContextValue::StyledStrs(vec![tip.into()]),
    );
    error
}

/// Where `by serve`, `by worker` or `by help serve|worker` hands over to
/// the server's own parser; see [`server_call`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerCall {
    /// How many leading arguments (after the program name) are `by`'s own
    /// to parse: global options, and the command unless `help`.
    pub prefix: usize,
    /// `by serve` or `by worker`, naming the server in its messages.
    pub program: &'static str,
    /// The server's arguments, `--worker` first for `by worker`.
    pub args: Vec<String>,
    /// `by help serve|worker`: print the server's help instead.
    pub help: bool,
}

/// `by [GLOBALS] serve|worker ARGS...`, `by [GLOBALS] help serve|worker`:
/// the server parses its own options (some share names with `by`'s global
/// ones, such as `--repo`), so they are handed over whole. `args` are after
/// the program name.
pub fn server_call(args: &[OsString]) -> Option<ServerCall> {
    let mut at = 0;
    while let Some(arg) = args.get(at).and_then(|a| a.to_str()) {
        let Some(long) = arg.strip_prefix("--") else {
            break;
        };
        let name = long.split_once('=').map_or(long, |(name, _)| name);
        if !["remote", "token-file", "repo", "ca-file"].contains(&name) {
            break;
        }
        at += if long.contains('=') { 1 } else { 2 };
    }
    let rest: Vec<&str> = args
        .get(at..)?
        .iter()
        .map(|a| a.to_str())
        .collect::<Option<_>>()?;
    let (help, name) = match rest.as_slice() {
        ["help", name, ..] => (true, *name),
        [name, ..] => (false, *name),
        [] => return None,
    };
    let program = match name {
        "serve" => "by serve",
        "worker" => "by worker",
        _ => return None,
    };
    let mut server_args = Vec::new();
    if name == "worker" {
        server_args.push("--worker".to_owned());
    }
    if !help {
        server_args.extend(rest[1..].iter().map(|a| (*a).to_owned()));
    }
    Some(ServerCall {
        prefix: if help { at } else { at + 1 },
        program,
        args: server_args,
        help,
    })
}

pub use branchyard_setup::config::split_words;

/// Quote `word` for a POSIX shell, leaving plain words as they are.
pub use branchyard_recipe::quote as shell_quote;

// Value parsers. Each error reads after "invalid value 'X' for '--flag
// <VALUE>': ".

pub(crate) fn non_blank(text: &str) -> Result<String, String> {
    match text.trim().is_empty() {
        true => Err("needs a value".into()),
        false => Ok(text.to_owned()),
    }
}

pub(crate) fn prompt_text(text: &str) -> Result<String, String> {
    match text.trim().is_empty() {
        true => Err("needs a prompt".into()),
        false => Ok(text.to_owned()),
    }
}

pub(crate) fn check_argv(line: &str) -> Result<Argv, String> {
    let argv = split_words(line)?;
    match argv.is_empty() {
        true => Err("needs a command".into()),
        false => Ok(Argv(argv)),
    }
}

pub(crate) fn command_argv(line: &str) -> Result<Argv, String> {
    let argv = split_words(line)?;
    match argv.is_empty() {
        true => Err("needs an executable".into()),
        false => Ok(Argv(argv)),
    }
}

/// Dollars, with or without a leading `$`.
pub(crate) fn usd(text: &str) -> Result<f64, String> {
    match text.trim().trim_start_matches('$').parse::<f64>() {
        Ok(usd) if usd.is_finite() && usd > 0.0 => Ok(usd),
        _ => Err("needs a positive number of dollars, such as 2.50".into()),
    }
}

pub(crate) fn positive_turns(text: &str) -> Result<u32, String> {
    match text.parse::<u32>() {
        Ok(turns) if turns > 0 => Ok(turns),
        _ => Err("needs a positive whole number".into()),
    }
}

pub(crate) fn delegate_depth(text: &str) -> Result<u32, String> {
    match text.parse::<u32>() {
        Ok(depth) if depth > 0 => Ok(depth),
        _ => Err("needs a positive whole number".into()),
    }
}

pub(crate) fn cpus(text: &str) -> Result<u8, String> {
    match text.parse::<u8>() {
        Ok(cpus) if cpus > 0 => Ok(cpus),
        _ => Err("needs a whole number from 1 to 255".into()),
    }
}

pub(crate) fn memory(text: &str) -> Result<u32, String> {
    match text.parse::<u32>() {
        Ok(mib) if mib > 0 => Ok(mib),
        _ => Err("needs a positive whole number of MiB".into()),
    }
}

/// Non-negative seconds.
pub(crate) fn seconds(text: &str) -> Result<f64, String> {
    match text.parse::<f64>() {
        Ok(secs) if secs.is_finite() && secs >= 0.0 => Ok(secs),
        _ => Err("needs a number of seconds, such as 30 or 2.5".into()),
    }
}

/// `text` as a duration: a number in `unit` seconds, or with a unit of its
/// own: `ms`, `s`, `m` or `h`. `None` unless finite and positive.
pub(crate) fn duration(text: &str, unit: f64) -> Option<Result<Duration, String>> {
    let text = text.trim();
    // A trailing unit only: "1e3" is a number.
    let number = text.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let suffix = &text[number.len()..];
    let scale = match suffix {
        "" => unit,
        "ms" => 0.001,
        "s" => 1.0,
        "m" => 60.0,
        "h" => 3600.0,
        _ => return None,
    };
    let value = number.trim().parse::<f64>().ok()?;
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    Some(Duration::try_from_secs_f64(value * scale).map_err(|_| "is too large".to_owned()))
}

pub(crate) fn minutes(text: &str) -> Result<Duration, String> {
    duration(text, 60.0).unwrap_or_else(|| {
        Err("needs a positive number of minutes, or a duration such as 90s or 2h".into())
    })
}

pub(crate) fn max_minutes(text: &str) -> Result<Duration, String> {
    minutes(text)
}

pub(crate) fn stall_after(text: &str) -> Result<Duration, String> {
    minutes(text)
}

/// `by report-status --ttl`: a duration such as `10m`; the engine checks
/// its bounds, so a bad one is refused `denied` as the tool's is.
pub(crate) fn report_ttl(text: &str) -> Result<Duration, String> {
    branchyard_support::time::parse_duration(text).map_err(|e| e.to_string())
}

pub(crate) fn watch_interval(text: &str) -> Result<Duration, String> {
    duration(text, 1.0)
        .and_then(Result::ok)
        .filter(|d| (0.05..=3600.0).contains(&d.as_secs_f64()))
        .ok_or_else(|| "needs a number of seconds from 0.05 to 3600, such as 0.5 or 250ms".into())
}

/// Split `claude-code,codex` into IDs. Duplicates are refused because
/// branch names derive from the harness.
pub(crate) fn harness_list(list: &str) -> Result<List, String> {
    let mut ids: Vec<String> = Vec::new();
    for id in list.split(',').map(str::trim) {
        if id.is_empty() {
            return Err(format!("has an empty entry in '{list}'"));
        }
        if ids.iter().any(|seen| seen == id) {
            return Err(format!("lists {id} twice"));
        }
        ids.push(id.to_owned());
    }
    Ok(List(ids))
}

pub(crate) fn variable_names(list: &str) -> Result<List, String> {
    let names: Vec<String> = list.split(',').map(|n| n.trim().to_owned()).collect();
    if names.iter().any(|n| n.is_empty() || n.contains('=')) {
        return Err("takes variable names, such as ANTHROPIC_API_KEY,GH_TOKEN".into());
    }
    Ok(List(names))
}

pub(crate) fn label(text: &str) -> Result<(String, String), String> {
    text.split_once('=')
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .ok_or_else(|| "needs KEY=VALUE".into())
}
