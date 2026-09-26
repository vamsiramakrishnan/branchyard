//! Command-line parsing for `by`.
//!
//! Hand-written to avoid a dependency. Each command declares its positionals
//! and flags once in [`COMMANDS`]; parsing and help text both read from it.

use std::fmt;

/// How tool permission requests are answered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Permissions {
    /// `--yes`: allow every request.
    Yes,
    /// `--ask`: prompt on the terminal.
    Ask,
    /// Neither flag: decided by whether a terminal is attached.
    #[default]
    Unset,
}

/// Options shared by `run`, `fan`, `send` and `fork`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TaskArgs {
    pub harness: Option<String>,
    pub name: Option<String>,
    pub base: Option<String>,
    /// Check argument vector, split from one quoted string.
    pub check: Option<Vec<String>>,
    pub budget_usd: Option<f64>,
    pub max_turns: Option<u32>,
    pub permissions: Permissions,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Run {
        prompt: String,
        task: TaskArgs,
    },
    Fan {
        prompt: String,
        harnesses: Vec<String>,
        task: TaskArgs,
    },
    Send {
        branch: String,
        prompt: String,
        task: TaskArgs,
    },
    Fork {
        branch: String,
        prompt: String,
        fresh_session: bool,
        task: TaskArgs,
    },
    Ls {
        json: bool,
    },
    Show {
        branch: String,
        json: bool,
    },
    Diff {
        branch: String,
    },
    Log {
        branch: String,
        json: bool,
    },
    Merge {
        branch: String,
        into: Option<String>,
    },
    Rm {
        branch: String,
    },
    Harnesses {
        json: bool,
    },
    /// General help, or one command's.
    Help {
        topic: Option<&'static Spec>,
    },
    Version,
}

/// A usage error: exit code 2.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageError {
    pub message: String,
    /// The command whose help would explain the mistake.
    pub command: Option<&'static str>,
}

impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

#[derive(Debug, PartialEq)]
pub struct Flag {
    pub long: &'static str,
    /// Value placeholder; `None` for a switch.
    pub value: Option<&'static str>,
    pub help: &'static str,
}

#[derive(Debug, PartialEq)]
pub struct Spec {
    pub name: &'static str,
    pub positionals: &'static [&'static str],
    pub summary: &'static str,
    pub flags: &'static [Flag],
}

impl Spec {
    pub fn usage(&self) -> String {
        let mut usage = format!("by {}", self.name);
        for positional in self.positionals {
            usage.push_str(&format!(" <{positional}>"));
        }
        if !self.flags.is_empty() {
            usage.push_str(" [options]");
        }
        usage
    }
}

const HARNESS: Flag = Flag {
    long: "harness",
    value: Some("ID"),
    help: "Harness or profile ID (default: claude-code)",
};
const HARNESS_LIST: Flag = Flag {
    long: "harness",
    value: Some("ID,ID,..."),
    help: "Harnesses to run on, one branch each (required)",
};
const NAME: Flag = Flag {
    long: "name",
    value: Some("NAME"),
    help: "Branch name (default: a slug of the prompt)",
};
const BASE: Flag = Flag {
    long: "base",
    value: Some("REV"),
    help: "Base revision (default: HEAD)",
};
const CHECK: Flag = Flag {
    long: "check",
    value: Some("\"CMD ARGS\""),
    help: "Check to pass before merging; split like a shell, run without one",
};
const BUDGET_USD: Flag = Flag {
    long: "budget-usd",
    value: Some("X"),
    help: "Stop once the harness's own cost estimate exceeds X dollars",
};
const MAX_TURNS: Flag = Flag {
    long: "max-turns",
    value: Some("N"),
    help: "Stop after N turns",
};
const YES: Flag = Flag {
    long: "yes",
    value: None,
    help: "Allow every tool permission request",
};
const ASK: Flag = Flag {
    long: "ask",
    value: None,
    help: "Ask on the terminal for each tool permission request",
};
const FRESH_SESSION: Flag = Flag {
    long: "fresh-session",
    value: None,
    help: "Start a new session if the harness cannot fork its conversation",
};
const JSON: Flag = Flag {
    long: "json",
    value: None,
    help: "Print JSON",
};
const INTO: Flag = Flag {
    long: "into",
    value: Some("TARGET"),
    help: "Local branch to merge into (default: the current branch)",
};

pub static COMMANDS: &[Spec] = &[
    Spec {
        name: "run",
        positionals: &["prompt"],
        summary: "Run a task on a new branch",
        flags: &[HARNESS, NAME, BASE, CHECK, BUDGET_USD, MAX_TURNS, YES, ASK],
    },
    Spec {
        name: "fan",
        positionals: &["prompt"],
        summary: "Run a task on several harnesses in parallel, then compare",
        flags: &[
            HARNESS_LIST,
            NAME,
            BASE,
            CHECK,
            BUDGET_USD,
            MAX_TURNS,
            YES,
            ASK,
        ],
    },
    Spec {
        name: "send",
        positionals: &["branch", "prompt"],
        summary: "Continue a branch's session with another prompt",
        flags: &[CHECK, BUDGET_USD, MAX_TURNS, YES, ASK],
    },
    Spec {
        name: "fork",
        positionals: &["branch", "prompt"],
        summary: "Start a new branch from a branch's candidate and conversation",
        flags: &[NAME, FRESH_SESSION, CHECK, BUDGET_USD, MAX_TURNS, YES, ASK],
    },
    Spec {
        name: "ls",
        positionals: &[],
        summary: "List branches",
        flags: &[JSON],
    },
    Spec {
        name: "show",
        positionals: &["branch"],
        summary: "Show one branch",
        flags: &[JSON],
    },
    Spec {
        name: "diff",
        positionals: &["branch"],
        summary: "Show a branch's candidate diff against its base",
        flags: &[],
    },
    Spec {
        name: "log",
        positionals: &["branch"],
        summary: "Show a branch's recorded events",
        flags: &[JSON],
    },
    Spec {
        name: "merge",
        positionals: &["branch"],
        summary: "Merge a branch's candidate after its check passes",
        flags: &[INTO],
    },
    Spec {
        name: "rm",
        positionals: &["branch"],
        summary: "Remove a branch's worktree and record",
        flags: &[],
    },
    Spec {
        name: "harnesses",
        positionals: &[],
        summary: "List harness profiles and whether they are installed",
        flags: &[JSON],
    },
    Spec {
        name: "help",
        positionals: &[],
        summary: "Show help for by or one command",
        flags: &[],
    },
];

pub fn spec(name: &str) -> Option<&'static Spec> {
    COMMANDS.iter().find(|spec| spec.name == name)
}

/// Parse the arguments after the program name.
pub fn parse(args: &[String]) -> Result<Command, UsageError> {
    let Some(first) = args.first() else {
        return Ok(Command::Help { topic: None });
    };
    match first.as_str() {
        "help" | "-h" | "--help" => return help(args.get(1..).unwrap_or_default()),
        "-V" | "--version" => return Ok(Command::Version),
        _ => {}
    }
    let spec = spec(first).ok_or_else(|| UsageError {
        message: format!("unknown command '{first}'"),
        command: None,
    })?;
    let m = Matches::parse(spec, &args[1..])?;
    if m.help {
        return Ok(Command::Help { topic: Some(spec) });
    }
    let mut positionals = m.positionals.iter().cloned();
    let mut next = || positionals.next().expect("arity checked in Matches::parse");
    Ok(match spec.name {
        "run" => Command::Run {
            prompt: next(),
            task: m.task()?,
        },
        "fan" => {
            let list = m
                .value("harness")
                .ok_or_else(|| m.error("--harness is required"))?;
            Command::Fan {
                prompt: next(),
                harnesses: harness_list(list).map_err(|e| m.error(e))?,
                task: m.task()?,
            }
        }
        "send" => Command::Send {
            branch: next(),
            prompt: next(),
            task: m.task()?,
        },
        "fork" => Command::Fork {
            branch: next(),
            prompt: next(),
            fresh_session: m.switch("fresh-session"),
            task: m.task()?,
        },
        "ls" => Command::Ls {
            json: m.switch("json"),
        },
        "show" => Command::Show {
            branch: next(),
            json: m.switch("json"),
        },
        "diff" => Command::Diff { branch: next() },
        "log" => Command::Log {
            branch: next(),
            json: m.switch("json"),
        },
        "merge" => Command::Merge {
            branch: next(),
            into: m.value("into").map(str::to_owned),
        },
        "rm" => Command::Rm { branch: next() },
        "harnesses" => Command::Harnesses {
            json: m.switch("json"),
        },
        other => unreachable!("command {other} has a spec but no parser"),
    })
}

fn help(rest: &[String]) -> Result<Command, UsageError> {
    match rest {
        [] => Ok(Command::Help { topic: None }),
        [topic] => match spec(topic) {
            Some(spec) => Ok(Command::Help { topic: Some(spec) }),
            None => Err(UsageError {
                message: format!("unknown command '{topic}'"),
                command: None,
            }),
        },
        [_, extra, ..] => Err(UsageError {
            message: format!("unexpected argument '{extra}'"),
            command: Some("help"),
        }),
    }
}

/// Raw flags and positionals, checked against a command's spec.
struct Matches {
    spec: &'static Spec,
    positionals: Vec<String>,
    flags: Vec<(&'static str, Option<String>)>,
    help: bool,
}

impl Matches {
    fn parse(spec: &'static Spec, args: &[String]) -> Result<Matches, UsageError> {
        let mut m = Matches {
            spec,
            positionals: Vec::new(),
            flags: Vec::new(),
            help: false,
        };
        let mut args = args.iter();
        let mut only_positionals = false;
        while let Some(arg) = args.next() {
            if only_positionals {
                m.positionals.push(arg.clone());
                continue;
            }
            match arg.as_str() {
                "--" => only_positionals = true,
                "-h" | "--help" => m.help = true,
                long if long.starts_with("--") => {
                    let (name, inline) = match long[2..].split_once('=') {
                        Some((name, value)) => (name, Some(value.to_owned())),
                        None => (&long[2..], None),
                    };
                    let flag = spec
                        .flags
                        .iter()
                        .find(|flag| flag.long == name)
                        .ok_or_else(|| m.error(format!("unknown option --{name}")))?;
                    if m.flags.iter().any(|(seen, _)| *seen == flag.long) {
                        return Err(m.error(format!("--{name} given twice")));
                    }
                    let value = match (flag.value, inline) {
                        (None, None) => None,
                        (None, Some(_)) => return Err(m.error(format!("--{name} takes no value"))),
                        (Some(_), Some(value)) => Some(value),
                        (Some(placeholder), None) => {
                            Some(args.next().cloned().ok_or_else(|| {
                                m.error(format!("--{name} needs a value {placeholder}"))
                            })?)
                        }
                    };
                    m.flags.push((flag.long, value));
                }
                short if short.starts_with('-') && short.len() > 1 => {
                    return Err(m.error(format!("unknown option {short}")));
                }
                _ => m.positionals.push(arg.clone()),
            }
        }
        if m.help {
            return Ok(m);
        }
        let expected = spec.positionals;
        if let Some(missing) = expected.get(m.positionals.len()) {
            return Err(m.error(format!("missing <{missing}>")));
        }
        if let Some(extra) = m.positionals.get(expected.len()) {
            let hint = if expected.contains(&"prompt") {
                " (quote a prompt that contains spaces)"
            } else {
                ""
            };
            return Err(m.error(format!("unexpected argument '{extra}'{hint}")));
        }
        Ok(m)
    }

    fn error(&self, message: impl Into<String>) -> UsageError {
        UsageError {
            message: message.into(),
            command: Some(self.spec.name),
        }
    }

    fn value(&self, name: &str) -> Option<&str> {
        self.flags
            .iter()
            .find(|(flag, _)| *flag == name)
            .and_then(|(_, value)| value.as_deref())
    }

    fn switch(&self, name: &str) -> bool {
        self.flags.iter().any(|(flag, _)| *flag == name)
    }

    fn task(&self) -> Result<TaskArgs, UsageError> {
        let permissions = match (self.switch("yes"), self.switch("ask")) {
            (true, true) => return Err(self.error("--yes and --ask conflict; pick one")),
            (true, false) => Permissions::Yes,
            (false, true) => Permissions::Ask,
            (false, false) => Permissions::Unset,
        };
        let check = match self.value("check") {
            None => None,
            Some(line) => {
                let argv = split_words(line).map_err(|e| self.error(format!("--check: {e}")))?;
                if argv.is_empty() {
                    return Err(self.error("--check needs a command"));
                }
                Some(argv)
            }
        };
        let budget_usd = match self.value("budget-usd") {
            None => None,
            Some(text) => match text.parse::<f64>() {
                Ok(usd) if usd.is_finite() && usd > 0.0 => Some(usd),
                _ => {
                    return Err(self.error(format!(
                        "--budget-usd needs a positive number of dollars, not '{text}'"
                    )))
                }
            },
        };
        let max_turns = match self.value("max-turns") {
            None => None,
            Some(text) => match text.parse::<u32>() {
                Ok(turns) if turns > 0 => Some(turns),
                _ => {
                    return Err(self.error(format!(
                        "--max-turns needs a positive whole number, not '{text}'"
                    )))
                }
            },
        };
        // `fan` reads `--harness` as a list; it is not one harness.
        let harness = match self.spec.name {
            "fan" => None,
            _ => self.value("harness").map(str::to_owned),
        };
        Ok(TaskArgs {
            harness,
            name: self.value("name").map(str::to_owned),
            base: self.value("base").map(str::to_owned),
            check,
            budget_usd,
            max_turns,
            permissions,
        })
    }
}

/// Split `claude-code,codex` into harness IDs. Duplicates are refused because
/// branch names derive from the harness.
fn harness_list(list: &str) -> Result<Vec<String>, String> {
    let mut harnesses: Vec<String> = Vec::new();
    for id in list.split(',').map(str::trim) {
        if id.is_empty() {
            return Err(format!("--harness has an empty entry in '{list}'"));
        }
        if harnesses.iter().any(|seen| seen == id) {
            return Err(format!("--harness lists {id} twice"));
        }
        harnesses.push(id.to_owned());
    }
    Ok(harnesses)
}

/// Split a command line into words the way a POSIX shell quotes them: single
/// quotes, double quotes and backslash escapes. There is no variable, glob
/// or operator expansion; the result runs without a shell.
pub fn split_words(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err("unterminated single quote".into()),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\' | '$' | '`')) => word.push(c),
                            Some(c) => {
                                word.push('\\');
                                word.push(c);
                            }
                            None => return Err("unterminated double quote".into()),
                        },
                        Some(c) => word.push(c),
                        None => return Err("unterminated double quote".into()),
                    }
                }
            }
            '\\' => {
                in_word = true;
                word.push(chars.next().ok_or("trailing backslash")?);
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

/// Quote `word` for a POSIX shell, leaving plain words as they are.
pub fn shell_quote(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:@%+=,".contains(c));
    if plain {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// Help for `by` itself.
pub fn general_help() -> String {
    let mut text = String::from(
        "by: delegate coding work to agent harnesses on git branches, and merge\n\
         only validated results.\n\nUsage: by <command> [options]\n\nCommands:\n",
    );
    for spec in COMMANDS {
        text.push_str(&format!("  {:<10} {}\n", spec.name, spec.summary));
    }
    text.push_str(
        "\nRun 'by help <command>' or 'by <command> --help' for its options.\n\
         Local mode: harnesses run as your operating-system user, with no other\n\
         isolation. State lives in .branchyard/ at the repository root.\n",
    );
    text
}

/// Help for one command.
pub fn command_help(spec: &Spec) -> String {
    let mut text = format!("{}\n\nUsage: {}\n", spec.summary, spec.usage());
    if !spec.flags.is_empty() {
        text.push_str("\nOptions:\n");
        let label = |flag: &Flag| match flag.value {
            Some(value) => format!("--{} {value}", flag.long),
            None => format!("--{}", flag.long),
        };
        let width = spec.flags.iter().map(|f| label(f).len()).max().unwrap_or(0);
        for flag in spec.flags {
            text.push_str(&format!("  {:<width$}  {}\n", label(flag), flag.help));
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(line: &str) -> Result<Command, UsageError> {
        parse(&split_words(line).unwrap())
    }

    fn err(line: &str) -> String {
        parse_str(line).unwrap_err().message
    }

    #[test]
    fn run_takes_every_task_option() {
        let command = parse_str(
            "run 'fix the flaky test' --harness codex --name flaky --base main \
             --check 'cargo test -p core' --budget-usd 2.5 --max-turns 3 --yes",
        )
        .unwrap();
        assert_eq!(
            command,
            Command::Run {
                prompt: "fix the flaky test".into(),
                task: TaskArgs {
                    harness: Some("codex".into()),
                    name: Some("flaky".into()),
                    base: Some("main".into()),
                    check: Some(vec![
                        "cargo".into(),
                        "test".into(),
                        "-p".into(),
                        "core".into()
                    ]),
                    budget_usd: Some(2.5),
                    max_turns: Some(3),
                    permissions: Permissions::Yes,
                },
            }
        );
    }

    #[test]
    fn run_defaults_and_equals_form() {
        let Command::Run { prompt, task } = parse_str("run --ask --name=x \"do it\"").unwrap()
        else {
            panic!("not run")
        };
        assert_eq!(prompt, "do it");
        assert_eq!(task.name.as_deref(), Some("x"));
        assert_eq!(task.permissions, Permissions::Ask);
        let Command::Run { task, .. } = parse_str("run go").unwrap() else {
            panic!("not run")
        };
        assert_eq!(task, TaskArgs::default());
    }

    #[test]
    fn fan_requires_a_harness_list() {
        let Command::Fan {
            harnesses, task, ..
        } = parse_str("fan go --harness 'claude-code, codex' --max-turns 2").unwrap()
        else {
            panic!("not fan")
        };
        assert_eq!(harnesses, ["claude-code", "codex"]);
        assert_eq!(task.harness, None);
        assert_eq!(task.max_turns, Some(2));
        assert_eq!(err("fan go"), "--harness is required");
        assert_eq!(
            err("fan go --harness codex,codex"),
            "--harness lists codex twice"
        );
        assert!(err("fan go --harness codex,").contains("empty entry"));
    }

    #[test]
    fn send_and_fork_take_a_branch_and_a_prompt() {
        assert_eq!(
            parse_str("send flaky 'now add a test' --yes").unwrap(),
            Command::Send {
                branch: "flaky".into(),
                prompt: "now add a test".into(),
                task: TaskArgs {
                    permissions: Permissions::Yes,
                    ..TaskArgs::default()
                },
            }
        );
        assert_eq!(
            parse_str("fork flaky 'try another way' --fresh-session --name alt").unwrap(),
            Command::Fork {
                branch: "flaky".into(),
                prompt: "try another way".into(),
                fresh_session: true,
                task: TaskArgs {
                    name: Some("alt".into()),
                    ..TaskArgs::default()
                },
            }
        );
        assert_eq!(err("send flaky"), "missing <prompt>");
        assert_eq!(err("fork"), "missing <branch>");
        assert_eq!(
            err("send flaky go --harness codex"),
            "unknown option --harness"
        );
    }

    #[test]
    fn inspection_commands() {
        assert_eq!(parse_str("ls").unwrap(), Command::Ls { json: false });
        assert_eq!(parse_str("ls --json").unwrap(), Command::Ls { json: true });
        assert_eq!(
            parse_str("show b --json").unwrap(),
            Command::Show {
                branch: "b".into(),
                json: true
            }
        );
        assert_eq!(
            parse_str("diff b").unwrap(),
            Command::Diff { branch: "b".into() }
        );
        assert_eq!(
            parse_str("log b").unwrap(),
            Command::Log {
                branch: "b".into(),
                json: false
            }
        );
        assert_eq!(
            parse_str("harnesses --json").unwrap(),
            Command::Harnesses { json: true }
        );
        assert_eq!(err("diff b --json"), "unknown option --json");
    }

    #[test]
    fn merge_and_rm() {
        assert_eq!(
            parse_str("merge b").unwrap(),
            Command::Merge {
                branch: "b".into(),
                into: None
            }
        );
        assert_eq!(
            parse_str("merge b --into release").unwrap(),
            Command::Merge {
                branch: "b".into(),
                into: Some("release".into())
            }
        );
        assert_eq!(
            parse_str("rm b").unwrap(),
            Command::Rm { branch: "b".into() }
        );
        assert_eq!(err("merge b --into"), "--into needs a value TARGET");
    }

    #[test]
    fn help_and_version() {
        assert_eq!(parse(&[]).unwrap(), Command::Help { topic: None });
        assert_eq!(parse_str("help").unwrap(), Command::Help { topic: None });
        assert_eq!(parse_str("--help").unwrap(), Command::Help { topic: None });
        assert_eq!(
            parse_str("help merge").unwrap(),
            Command::Help {
                topic: spec("merge")
            }
        );
        // --help wins over otherwise invalid arguments.
        assert_eq!(
            parse_str("run --help").unwrap(),
            Command::Help { topic: spec("run") }
        );
        assert_eq!(
            parse_str("fan -h").unwrap(),
            Command::Help { topic: spec("fan") }
        );
        assert_eq!(parse_str("--version").unwrap(), Command::Version);
        assert_eq!(parse_str("-V").unwrap(), Command::Version);
        assert_eq!(err("help nope"), "unknown command 'nope'");
    }

    #[test]
    fn flag_errors_name_the_command() {
        let error = parse_str("run go --bogus").unwrap_err();
        assert_eq!(error.message, "unknown option --bogus");
        assert_eq!(error.command, Some("run"));
        assert_eq!(err("nope"), "unknown command 'nope'");
        assert_eq!(err("run go -x"), "unknown option -x");
        assert_eq!(
            err("run go --yes --ask"),
            "--yes and --ask conflict; pick one"
        );
        assert_eq!(err("run go --yes=1"), "--yes takes no value");
        assert_eq!(err("run go --name a --name b"), "--name given twice");
        assert_eq!(err("run go --harness"), "--harness needs a value ID");
        assert!(err("run go --budget-usd -1").contains("positive number"));
        assert!(err("run go --budget-usd NaN").contains("positive number"));
        assert!(err("run go --max-turns 0").contains("positive whole number"));
        assert!(err("run go --max-turns 1.5").contains("positive whole number"));
        assert_eq!(err("run go --check ''"), "--check needs a command");
        assert_eq!(
            err("run go --check '\"cargo'"),
            "--check: unterminated double quote"
        );
        assert_eq!(
            err("run fix the test"),
            "unexpected argument 'the' (quote a prompt that contains spaces)"
        );
        assert_eq!(err("ls extra"), "unexpected argument 'extra'");
    }

    #[test]
    fn double_dash_allows_prompts_that_look_like_flags() {
        let Command::Run { prompt, task } = parse_str("run --yes -- --explain-flags").unwrap()
        else {
            panic!("not run")
        };
        assert_eq!(prompt, "--explain-flags");
        assert_eq!(task.permissions, Permissions::Yes);
    }

    #[test]
    fn split_words_follows_shell_quoting() {
        let split = |s: &str| split_words(s).unwrap();
        assert_eq!(split("  cargo   test  "), ["cargo", "test"]);
        assert_eq!(split("sh -c 'echo $HOME'"), ["sh", "-c", "echo $HOME"]);
        assert_eq!(
            split(r#"say "a \"quoted\" \$word" and\ more"#),
            ["say", r#"a "quoted" $word"#, "and more"]
        );
        assert_eq!(split(r#""keep \n literal""#), [r"keep \n literal"]);
        assert_eq!(split("''"), [""]);
        assert_eq!(split("a'b'\"c\""), ["abc"]);
        assert!(split("").is_empty());
        assert_eq!(split_words("a\\").unwrap_err(), "trailing backslash");
        assert_eq!(split_words("'a").unwrap_err(), "unterminated single quote");
    }

    #[test]
    fn shell_quote_round_trips() {
        assert_eq!(shell_quote("fix-flaky_1.2"), "fix-flaky_1.2");
        assert_eq!(shell_quote("it's here"), r"'it'\''s here'");
        assert_eq!(shell_quote(""), "''");
        for word in ["plain", "two words", "it's", "$x", ""] {
            assert_eq!(split_words(&shell_quote(word)).unwrap(), [word]);
        }
    }

    #[test]
    fn help_lists_every_command_and_option() {
        let general = general_help();
        for spec in COMMANDS {
            assert!(general.contains(spec.name), "{}", spec.name);
            let help = command_help(spec);
            assert!(help.contains(&spec.usage()));
            for flag in spec.flags {
                assert!(help.contains(&format!("--{}", flag.long)));
            }
        }
        assert_eq!(
            spec("fork").unwrap().usage(),
            "by fork <branch> <prompt> [options]"
        );
        assert_eq!(spec("ls").unwrap().usage(), "by ls [options]");
        assert_eq!(spec("rm").unwrap().usage(), "by rm <branch>");
    }
}
