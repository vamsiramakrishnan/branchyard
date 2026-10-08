//! The delegation commands' help, from [`branchyard::operations`]: which
//! flags a harness may not pass and why, what a person alone may do, and a
//! usage line that keeps an optional branch optional. The tests here are
//! the CLI's side of the parity checks: every operation's subcommand and
//! flags exist, the subcommands have no flag the table does not list, and
//! the delegation skill and `docs/delegation.md` mention only commands and
//! flags that exist.

use branchyard::operations::{Context, Operation, OPERATIONS};

/// `cmd` with each delegation command's help annotated from the table.
pub fn annotate(mut cmd: clap::Command) -> clap::Command {
    for operation in OPERATIONS {
        cmd = at_path(cmd, operation.cli, &|sub| annotate_one(sub, operation));
    }
    cmd
}

/// Apply `f` to the subcommand at `path` below `cmd`.
fn at_path(
    cmd: clap::Command,
    path: &[&str],
    f: &dyn Fn(clap::Command) -> clap::Command,
) -> clap::Command {
    match path.split_first() {
        None => f(cmd),
        Some((first, rest)) => {
            if cmd.find_subcommand(first).is_none() {
                return cmd;
            }
            cmd.mut_subcommand(*first, |sub| at_path(sub, rest, f))
        }
    }
}

/// The argument a table entry names: `--flag` by its long name, `<NAME>`
/// by its id.
pub(crate) fn find<'a>(sub: &'a clap::Command, cli: &str) -> Option<&'a clap::Arg> {
    match cli.strip_prefix("--") {
        Some(long) => sub.get_arguments().find(|a| a.get_long() == Some(long)),
        None => {
            let id = cli
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_lowercase();
            sub.get_positionals().find(|a| a.get_id().as_str() == id)
        }
    }
}

fn annotate_one(mut sub: clap::Command, operation: &Operation) -> clap::Command {
    // A flag shared by two operations (by send's --steer) is annotated
    // once, by the subcommand's own operation.
    if operation.selector.is_some() {
        return sub;
    }
    let mut notes = Vec::new();
    for param in operation.params {
        let (Some(cli), Some(why)) = (param.cli, param.inside) else {
            continue;
        };
        let Some(arg) = find(&sub, cli) else {
            continue;
        };
        let id = arg.get_id().clone();
        let help = arg.get_help().map(|h| h.to_string()).unwrap_or_default();
        let note = format!("refused inside a harness: {why}");
        sub = sub.mut_arg(id, |a| a.help(format!("{help} [{note}]")));
    }
    if let Some(why) = operation.person_flags {
        let accepted: Vec<&str> = operation.params.iter().filter_map(|p| p.cli).collect();
        notes.push(format!(
            "Inside a harness, {} takes only {}: {why}.",
            operation.command(),
            accepted.join(", ")
        ));
    }
    if let Context::No(why) = operation.contexts.inside {
        notes.push(format!("Not inside a harness: {why}."));
    }
    if let Context::No(why) = operation.contexts.remote {
        notes.push(format!("Not with --remote: {why}."));
    }
    if !notes.is_empty() {
        let before = sub
            .get_after_help()
            .map(|h| format!("{h}\n\n"))
            .unwrap_or_default();
        sub = sub.after_help(format!("{before}{}", notes.join("\n")));
    }
    // clap prints the usage of an error from the arguments given, so an
    // optional branch someone passed shows as required (`<BRANCH>`); the
    // whole usage keeps it `[BRANCH]`.
    if sub
        .get_positionals()
        .any(|a| !a.is_required_set() && !a.is_hide_set())
    {
        let usage = usage(&sub, operation.cli);
        sub = sub.override_usage(usage);
    }
    sub
}

/// `by inspect [OPTIONS] [BRANCH]`: the subcommand's whole usage.
fn usage(sub: &clap::Command, path: &[&str]) -> String {
    let mut words = vec!["by".to_owned()];
    words.extend(path.iter().map(|w| (*w).to_owned()));
    words.push("[OPTIONS]".into());
    for arg in sub.get_positionals().filter(|a| !a.is_hide_set()) {
        let name = arg
            .get_value_names()
            .and_then(|names| names.first())
            .map_or_else(|| arg.get_id().as_str().to_uppercase(), |n| n.to_string());
        let many = arg.get_num_args().is_some_and(|n| n.max_values() > 1);
        let dots = if many { "..." } else { "" };
        words.push(match arg.is_required_set() {
            true => format!("<{name}>{dots}"),
            false => format!("[{name}]{dots}"),
        });
    }
    words.join(" ")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn built() -> clap::Command {
        let mut cmd = crate::args::command();
        cmd.build();
        cmd
    }

    fn subcommand<'a>(cmd: &'a clap::Command, path: &[&str]) -> Option<&'a clap::Command> {
        path.iter()
            .try_fold(cmd, |cmd, word| cmd.find_subcommand(word))
    }

    /// Every operation's subcommand exists with every flag the table
    /// lists, and has no flag it does not: a flag added to one surface
    /// fails here until the table says what the others call it.
    #[test]
    fn every_operation_has_its_command_and_only_its_flags() {
        let cmd = built();
        for operation in OPERATIONS {
            let sub = subcommand(&cmd, operation.cli)
                .unwrap_or_else(|| panic!("{} has no `{}`", operation.name, operation.command()));
            if let Some(selector) = operation.selector {
                assert!(find(sub, selector).is_some(), "{}", operation.command());
            }
            for param in operation.params {
                if let Some(cli) = param.cli {
                    assert!(
                        find(sub, cli).is_some(),
                        "`{}` has no {cli}",
                        operation.command()
                    );
                }
            }
            if operation.selector.is_some() || operation.person_flags.is_some() {
                continue;
            }
            for arg in sub.get_arguments() {
                if arg.is_global_set() || ["help", "version"].contains(&arg.get_id().as_str()) {
                    continue;
                }
                let listed =
                    operation.params.iter().filter_map(|p| p.cli).any(|cli| {
                        find(sub, cli).is_some_and(|found| found.get_id() == arg.get_id())
                    });
                assert!(
                    listed,
                    "`{}` has {} ({:?}), which branchyard::operations does not list",
                    operation.command(),
                    arg.get_id(),
                    arg.get_long()
                );
            }
        }
    }

    /// What a person starts a root with, a parent may give a child.
    #[test]
    fn run_and_spawn_share_their_task_flags() {
        let cmd = built();
        for command in ["run", "spawn"] {
            let sub = cmd.find_subcommand(command).unwrap();
            for flag in branchyard::operations::SHARED_TASK_FLAGS {
                assert!(find(sub, flag).is_some(), "by {command} has no {flag}");
            }
        }
    }

    #[test]
    fn help_says_which_flags_a_harness_may_not_pass() {
        let cmd = built();
        let spawn = cmd.find_subcommand("spawn").unwrap();
        let yes = find(spawn, "--yes")
            .unwrap()
            .get_help()
            .unwrap()
            .to_string();
        assert!(yes.contains("refused inside a harness"), "{yes}");
        assert!(yes.contains("only a person answers"), "{yes}");
        let send = cmd.find_subcommand("send").unwrap();
        let after = send.get_after_help().unwrap().to_string();
        assert!(
            after.contains("Inside a harness, by send takes only"),
            "{after}"
        );
        let discard = cmd.find_subcommand("discard").unwrap();
        let after = discard.get_after_help().unwrap().to_string();
        assert!(after.contains("Not with --remote"), "{after}");
    }

    /// A command line from the docs: its subcommand path and its flags.
    fn mentioned(line: &str) -> Option<(Vec<String>, Vec<String>)> {
        let words: Vec<&str> = line.split_whitespace().collect();
        let start = words.iter().position(|w| *w == "by")?;
        let cmd = built();
        let mut path = Vec::new();
        let mut at = &cmd;
        for word in &words[start + 1..] {
            match at.find_subcommand(word) {
                Some(sub) => {
                    path.push((*word).to_owned());
                    at = sub;
                }
                None => break,
            }
        }
        if path.is_empty() {
            return None;
        }
        let flags = words[start + 1..]
            .iter()
            .filter_map(|w| {
                let w = w.trim_matches(|c: char| "`[]|,.;:()".contains(c));
                let flag = w.split('=').next()?;
                (flag.starts_with("--") && flag.len() > 2).then(|| flag.to_owned())
            })
            .collect();
        Some((path, flags))
    }

    /// Spans of `text` that are code: fenced blocks' lines and inline
    /// backticks, each with the paragraph it came from, or `None` for a
    /// table's cell (whose columns say different things).
    fn code_spans(text: &str) -> Vec<(String, Option<usize>)> {
        let mut spans = Vec::new();
        let mut fenced = false;
        // A paragraph, or an item of a list: a scope a bare flag belongs to.
        let mut scope = 0;
        for block in text.split("\n\n") {
            scope += 1;
            for line in block.lines() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("```") {
                    fenced = !fenced;
                    continue;
                }
                if fenced {
                    spans.push((line.to_owned(), Some(scope)));
                    continue;
                }
                if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
                    scope += 1;
                }
                let at = (!trimmed.starts_with('|')).then_some(scope);
                for (i, span) in line.split('`').enumerate() {
                    if i % 2 == 1 {
                        spans.push((span.to_owned(), at));
                    }
                }
            }
        }
        spans
    }

    /// Every `by ...` the text shows names a subcommand that exists with
    /// flags it takes, and a bare `--flag` belongs to the last command
    /// named in the same paragraph.
    fn check_commands(name: &str, text: &str) -> Vec<String> {
        let cmd = built();
        let mut problems = Vec::new();
        let mut last: Option<(Vec<String>, Option<usize>)> = None;
        for (span, paragraph) in code_spans(text) {
            let span = span.trim();
            if let Some((path, flags)) = mentioned(span) {
                // Their options are the server's own parser's.
                if ["serve", "worker"].contains(&path[0].as_str()) {
                    continue;
                }
                let words: Vec<&str> = path.iter().map(String::as_str).collect();
                let sub = subcommand(&cmd, &words).unwrap();
                for flag in flags {
                    if find(sub, &flag).is_none() {
                        problems.push(format!("{name}: `by {}` has no {flag}", path.join(" ")));
                    }
                }
                last = Some((path, paragraph));
                continue;
            }
            // `by <command> --help` stands for every command.
            if span.starts_with("by <") {
                continue;
            }
            if span.starts_with("by ") && !span.starts_with("by --") {
                problems.push(format!("{name}: `{span}` is not a by command"));
                continue;
            }
            let flag = span.split([' ', '=']).next().unwrap_or_default();
            if !flag.starts_with("--") || flag.len() <= 2 {
                continue;
            }
            match &last {
                Some((path, at)) if at.is_some() && *at == paragraph => {
                    let words: Vec<&str> = path.iter().map(String::as_str).collect();
                    let sub = subcommand(&cmd, &words).unwrap();
                    if find(sub, flag).is_none() {
                        problems.push(format!(
                            "{name}: `{flag}` follows `by {}`, which has no such flag",
                            path.join(" ")
                        ));
                    }
                }
                _ => {}
            }
        }
        problems
    }

    fn read(path: &str) -> String {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        std::fs::read_to_string(root.join(path)).unwrap()
    }

    /// The skill and the leaf's note are what a harness reads to use `by`:
    /// a command or flag they name that does not exist is one a model will
    /// try (`by inspect --wait` was, in six runs out of ten).
    #[test]
    fn the_skill_names_only_commands_and_flags_that_exist() {
        let mut problems = Vec::new();
        for path in [
            "plugins/branchyard/skills/delegate/SKILL.md",
            "plugins/branchyard/leaf.md",
            "docs/delegation.md",
            "docs/storage.md",
        ] {
            problems.extend(check_commands(path, &read(path)));
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn the_checker_finds_a_flag_a_command_lacks() {
        let text = "Watch with `by inspect <child>`. Wait with `--wait` or poll.\n\n\
                    `by spawn x --wait` and `by artifact publish f --media-type t`.\n\n\
                    `by inspect --cursor 1`";
        let problems = check_commands("t", text);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].contains("`--wait` follows `by inspect`"));
        assert!(problems[1].contains("`by inspect` has no --cursor"));
    }

    /// The delegation table in `docs/delegation.md` lists every command
    /// a harness can run, and the MCP tools paragraph every tool.
    #[test]
    fn the_delegation_doc_lists_every_operation() {
        let doc = read("docs/delegation.md");
        let mut missing = BTreeSet::new();
        for operation in OPERATIONS {
            if operation.contexts.inside != Context::Yes {
                continue;
            }
            let command = format!("`by {}", operation.cli.join(" "));
            let selected = operation.selector.is_none_or(|flag| doc.contains(flag));
            if !doc.contains(&command) || !selected {
                missing.insert(operation.command());
            }
            if let Some(tool) = operation.tool {
                if !doc.contains(&format!("`{tool}`")) {
                    missing.insert(format!("MCP {tool}"));
                }
            }
        }
        assert!(missing.is_empty(), "docs/delegation.md lacks {missing:?}");
    }

    /// Models guessed the edit JSON from errors ("missing field kind",
    /// "unknown field branch"); the help shows it.
    #[test]
    fn graph_apply_help_shows_the_edit_format() {
        let cmd = built();
        let apply = subcommand(&cmd, &["graph", "apply"]).unwrap();
        let help = apply.get_after_help().unwrap().to_string();
        for needle in [
            "\"kind\": \"spawn\"",
            "\"kind\": \"add_dependency\", \"dependent\": \"B\", \"prerequisite\": \"A\"",
            "remove_dependency",
            "expected_revision",
            "by graph apply --expected-revision 3 --edits",
        ] {
            assert!(help.contains(needle), "{needle} missing from\n{help}");
        }
        // The example parses as the proposal it shows.
        let start = help.find("--edits '").unwrap() + "--edits '".len();
        let edits = &help[start..start + help[start..].find('\'').unwrap()];
        let edits: Vec<branchyard::GraphEdit> = serde_json::from_str(edits).unwrap();
        assert_eq!(edits.len(), 2);
    }

    /// `--mcp NAME=https://URL` is an HTTP server, and `--mcp-header` gives
    /// it a header from a secret that is never stored.
    #[test]
    fn mcp_takes_an_http_server_with_headers() {
        let parsed = crate::args::parse_from([
            "by",
            "run",
            "go",
            "--mcp",
            "docs=/usr/bin/docs-mcp --stdio",
            "--mcp",
            "search=https://mcp.example.com/mcp",
            "--mcp-header",
            "search:Authorization=SEARCH_AUTH",
            "--mcp-header",
            "search:X-Tenant=@/run/tenant",
        ])
        .unwrap();
        let Some(crate::args::Command::Run { task, .. }) = parsed.command else {
            panic!("not run");
        };
        let spec = task.into_inner().provision.unwrap();
        assert_eq!(spec.mcp_servers[0].name, "docs");
        let search = &spec.remote_mcp_servers[0];
        assert_eq!(search.url, "https://mcp.example.com/mcp");
        assert_eq!(search.headers["Authorization"], "SEARCH_AUTH");
        assert_eq!(search.headers["X-Tenant"], "MCP_SEARCH_X_TENANT");
        let secrets: Vec<&str> = spec.secrets.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(secrets, ["SEARCH_AUTH", "MCP_SEARCH_X_TENANT"]);
        let refused = crate::args::parse_from([
            "by",
            "run",
            "go",
            "--mcp",
            "search=https://mcp.example.com/mcp",
            "--mcp-header",
            "other:Authorization=VAR",
        ]);
        assert!(refused.unwrap_err().to_string().contains("no --mcp other="));
    }

    /// clap shows an error's usage from the arguments given; an optional
    /// branch someone passed must still read as optional.
    #[test]
    fn an_optional_branch_stays_optional_in_an_errors_usage() {
        let error = crate::args::parse_from(["by", "inspect", "units", "--wait"]).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("by inspect [OPTIONS] [BRANCH]"), "{text}");
        assert!(!text.contains("<BRANCH>"), "{text}");
    }
}
