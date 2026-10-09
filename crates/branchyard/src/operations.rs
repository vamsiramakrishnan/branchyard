//! The delegation operations, and every surface that reaches each one.
//!
//! One table, [`OPERATIONS`], says for each operation what it is called on
//! every surface: the `by` subcommand and its flags, the MCP tool and its
//! arguments, the Python function and its keyword arguments, and the
//! [`crate::Delegate`] method. It also says where the operation may run
//! (inside a harness, outside one, through a server), which flags a harness
//! may not pass and why, and which [`Capability`] a harness's token needs
//! for it.
//!
//! The table is checked, not trusted. Parity tests in each crate fail when
//! a surface lacks an operation or an argument the table lists, or has one
//! the table does not:
//!
//! - `branchyard` (this crate): every tool is dispatched, every Rust method
//!   exists, and every Python function takes the keyword arguments listed
//!   (`sdk/python/branchyard.py`).
//! - `branchyard-mcp`: the server lists exactly these tools, each with
//!   exactly these arguments.
//! - `branchyard-cli`: each subcommand exists with exactly these flags, and
//!   the delegation skill and `docs/delegation.md` mention only commands
//!   and flags that exist. The CLI's `--help` notes for flags refused inside
//!   a harness come from [`Param::inside`].
//!
//! The engine checks a token's capability against [`Operation::capability`]
//! for every call through the broker, so a leaf's token (one whose envelope
//! allows no children) reaches its own branch, its storage and its parent's
//! inbox, and nothing else.
//!
//! To add an operation: add a row here, then follow the failing tests.

use serde::Serialize;

/// What a harness's token must allow for an operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Read the branch itself: `inspect`, `events`, `graph` without a
    /// target, `children`. Every token has it.
    Own,
    /// Artifacts and scratch areas the branch may reach under
    /// `docs/storage.md`'s grants. Every token has it.
    Storage,
    /// Message the branch's parent (`ask`, `report`, `escalate`) and read
    /// its own inbox. Every token has it.
    Message,
    /// Create children and act on descendants. Only a token whose
    /// envelope's `max_depth` is above 0 has it.
    Delegate,
}

impl Capability {
    /// Whether a token for a branch that `can_spawn` (or not) holds this.
    pub fn held(self, can_spawn: bool) -> bool {
        self != Capability::Delegate || can_spawn
    }
}

/// Whether an operation runs in a context, or why not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Context {
    /// It runs there.
    Yes,
    /// It does not, for this reason.
    No(&'static str),
}

/// Where an operation may run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Contexts {
    /// Run by a harness on its own branch, with its token.
    pub inside: Context,
    /// Run by a person (or a program) on a local repository.
    pub outside: Context,
    /// Run by a person through a server: `by --remote`, `branchyard-client`.
    pub remote: Context,
}

const EVERYWHERE: Contexts = Contexts {
    inside: Context::Yes,
    outside: Context::Yes,
    remote: Context::Yes,
};

/// One argument of an operation, as each surface spells it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Param {
    /// The `by` flag (`--budget-usd`) or positional value (`<BRANCH>`, as
    /// clap names it in usage), or `None` when the CLI lacks it.
    pub cli: Option<&'static str>,
    /// The MCP tool's argument, a path into its JSON for a nested one
    /// (`budget.max_usd`), or `None` when the tool lacks it.
    pub tool: Option<&'static str>,
    /// The Python function's keyword argument, or `None`.
    pub python: Option<&'static str>,
    /// Why a surface lacks it, when one does. The parity tests require a
    /// reason for every gap.
    pub why: &'static str,
    /// Refused inside a harness, and why; shown in `by <command> --help`.
    pub inside: Option<&'static str>,
}

impl Param {
    const fn all(cli: &'static str, tool: &'static str, python: &'static str) -> Param {
        Param {
            cli: Some(cli),
            tool: Some(tool),
            python: Some(python),
            why: "",
            inside: None,
        }
    }

    /// A flag only the CLI has, with the reason.
    const fn cli(cli: &'static str, why: &'static str) -> Param {
        Param {
            cli: Some(cli),
            tool: None,
            python: None,
            why,
            inside: None,
        }
    }

    /// An argument only the MCP tool has, with the reason.
    const fn tool(tool: &'static str, why: &'static str) -> Param {
        Param {
            cli: None,
            tool: Some(tool),
            python: None,
            why,
            inside: None,
        }
    }

    /// A flag only a person outside a harness may pass, with the reason.
    const fn outside(cli: &'static str, why: &'static str) -> Param {
        Param {
            cli: Some(cli),
            tool: None,
            python: None,
            why,
            inside: Some(why),
        }
    }

    const fn without_python(mut self, why: &'static str) -> Param {
        self.python = None;
        self.why = why;
        self
    }
}

/// Every surface's `--json`.
const JSON: Param = Param::cli(
    "--json",
    "every tool result and Python return value is already JSON",
);
const ACT_AS: Param = Param::outside(
    "--as",
    "inside a harness the acting branch is always the harness's own, from its token",
);
const ACTING_BRANCH: Param = Param::outside(
    "--branch",
    "inside a harness the acting branch is always the harness's own, from its token",
);
const TARGET: Param = Param::all("<BRANCH>", "branch", "branch");
const PROMPT_FILE: Param = Param::cli(
    "--prompt-file",
    "a tool call and a Python string carry a prompt of any length; only a shell command line \
     needs a file (Python passes its prompt to by on stdin)",
);
/// The tool's flat limits, beside its nested `budget`.
const FLAT_BUDGET: &str = "the tool also takes the flat limits the CLI and Python name \
                           (budget_usd, max_turns, max_minutes) in place of budget; given both, \
                           it is refused";
const PERMISSIONS: &str = "a child runs under its parent's policy, and only a person answers \
                           permission requests";

/// A delegation operation and its name on each surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Operation {
    /// The operation's name, used in events and errors.
    pub name: &'static str,
    /// The `by` subcommand path, such as `["artifact", "publish"]`.
    pub cli: &'static [&'static str],
    /// A flag that selects this operation within a shared subcommand, such
    /// as `--steer` on `by send`.
    pub selector: Option<&'static str>,
    /// Another `by` subcommand that runs this operation alone, such as
    /// `by steer` for `by send --steer`. It takes the same `params`.
    pub alias: Option<&'static [&'static str]>,
    /// The MCP tool, or `None` with [`Operation::why`].
    pub tool: Option<&'static str>,
    /// The Python function, or `None` with [`Operation::why`].
    pub python: Option<&'static str>,
    /// The [`crate::Delegate`] method, or `None` with [`Operation::why`].
    pub rust: Option<&'static str>,
    /// Why a surface lacks the operation, when one does.
    pub why: &'static str,
    /// What a harness's token needs to call it.
    pub capability: Capability,
    /// Where it may run.
    pub contexts: Contexts,
    /// Its arguments, as each surface spells them.
    pub params: &'static [Param],
    /// Flags of the CLI subcommand not listed in `params`: they configure
    /// the turn a person runs in the foreground (limits, permissions,
    /// provisioning), and a harness may not pass them, for this reason.
    pub person_flags: Option<&'static str>,
}

const fn op(
    name: &'static str,
    cli: &'static [&'static str],
    tool: &'static str,
    python: &'static str,
    rust: &'static str,
    capability: Capability,
    params: &'static [Param],
) -> Operation {
    Operation {
        name,
        cli,
        selector: None,
        alias: None,
        tool: Some(tool),
        python: Some(python),
        rust: Some(rust),
        why: "",
        capability,
        contexts: EVERYWHERE,
        params,
        person_flags: None,
    }
}

impl Operation {
    const fn person_flags(mut self, why: &'static str) -> Operation {
        self.person_flags = Some(why);
        self
    }

    const fn selector(mut self, flag: &'static str) -> Operation {
        self.selector = Some(flag);
        self
    }

    const fn alias(mut self, cli: &'static [&'static str]) -> Operation {
        self.alias = Some(cli);
        self
    }

    const fn local_only(mut self, why: &'static str) -> Operation {
        self.tool = None;
        self.python = None;
        self.why = why;
        self.contexts = Contexts {
            inside: Context::No(why),
            outside: Context::Yes,
            remote: Context::No(why),
        };
        self
    }

    const fn not_remote(mut self, why: &'static str) -> Operation {
        self.contexts.remote = Context::No(why);
        self
    }

    const fn no_python(mut self, why: &'static str) -> Operation {
        self.python = None;
        self.why = why;
        self
    }

    /// The `by` command line's words, such as `by artifact publish` or
    /// `by send --steer`.
    pub fn command(&self) -> String {
        let mut words = vec!["by"];
        words.extend_from_slice(self.cli);
        if let Some(selector) = self.selector {
            words.push(selector);
        }
        words.join(" ")
    }
}

/// The operations, in the order the MCP server lists its tools.
///
pub const OPERATIONS: &[Operation] = &[
    op(
        "spawn",
        &["spawn"],
        "spawn",
        "spawn",
        "spawn",
        Capability::Delegate,
        &[
            Param::all("<PROMPT>", "prompt", "prompt"),
            Param::all("--harness", "harness", "harness"),
            Param::all("--name", "name", "name"),
            Param::all("--base", "base", "base"),
            Param::all("--budget-usd", "budget.max_usd", "budget_usd"),
            Param::all("--max-turns", "budget.max_turns", "max_turns"),
            Param::all("--max-minutes", "budget.max_minutes", "max_minutes"),
            Param::tool("budget_usd", FLAT_BUDGET),
            Param::tool("max_turns", FLAT_BUDGET),
            Param::tool("max_minutes", FLAT_BUDGET),
            Param::all("--check", "check", "check"),
            Param::all("--max-depth", "max_depth", "max_depth"),
            Param::all("--max-children", "max_children", "max_children"),
            Param::all("--harnesses", "harnesses", "harnesses"),
            Param::all("--deny", "deny", "deny"),
            Param::all("--seat", "seat", "seat"),
            Param::all("--depends-on", "depends_on", "depends_on"),
            Param::all("--after", "after", "after"),
            Param::all("--bind", "bindings", "bindings"),
            Param::all("--connector", "connectors", "connectors"),
            Param::all("--plan", "plan", "plan"),
            Param::all("--model", "model", "model"),
            PROMPT_FILE,
            Param::cli(
                "--issue",
                "reads the issue with gh on the caller's machine; elsewhere, put it in the prompt",
            ),
            Param::cli(
                "--wait",
                "a tool call should not block on a child's turn: poll inspect, or Python's wait",
            ),
            Param::cli(
                "--stall-after",
                "a stall window is the caller's choice for a turn it watches",
            ),
            Param::cli(
                "--stall-action",
                "a stall window is the caller's choice for a turn it watches",
            ),
            JSON,
            Param::outside(
                "--parent",
                "inside a harness the parent is always the harness's own branch",
            ),
            Param::outside("--yes", PERMISSIONS),
            Param::outside("--ask", PERMISSIONS),
            Param::outside("--permissions", PERMISSIONS),
            Param::outside(
                "--allow-unapproved-tools",
                "a child keeps its parent's approval routing",
            ),
        ],
    ),
    op(
        "inspect",
        &["inspect"],
        "inspect",
        "inspect",
        "inspect",
        Capability::Own,
        &[TARGET, JSON],
    ),
    op(
        "events",
        &["events"],
        "events",
        "events",
        "events",
        Capability::Own,
        &[
            TARGET,
            Param::all("--cursor", "cursor", "cursor"),
            Param::all("--limit", "limit", "limit"),
            JSON,
        ],
    ),
    op(
        "send",
        &["send"],
        "send",
        "send",
        "send",
        Capability::Delegate,
        &[
            TARGET,
            Param::all("<PROMPT>", "prompt", "prompt"),
            PROMPT_FILE,
            Param::all("--retry", "retry", "retry"),
            Param::cli(
                "--wait",
                "a tool call should not block on a child's turn: poll inspect, or Python's wait",
            ),
            JSON,
        ],
    )
    .person_flags(
        "inside a harness a descendant keeps its own limits and policy; only a person runs a \
         turn in the foreground with new ones",
    ),
    op(
        "steer",
        &["send"],
        "steer",
        "steer",
        "steer",
        Capability::Delegate,
        &[
            TARGET,
            Param::all("<PROMPT>", "text", "text"),
            PROMPT_FILE,
            JSON,
        ],
    )
    .selector("--steer")
    .alias(&["steer"]),
    op(
        "integrate",
        &["integrate"],
        "propose_integration",
        "integrate",
        "integrate",
        Capability::Delegate,
        &[
            Param::all("<BRANCHES>", "branches", "*branches"),
            Param {
                cli: None,
                tool: Some("branch"),
                python: None,
                why: "the tool's shorthand for one branch; by integrate and Python take one or \
                      more",
                inside: None,
            },
            JSON,
        ],
    ),
    op(
        "check",
        &["check"],
        "check",
        "check",
        "check",
        Capability::Own,
        &[TARGET, JSON],
    )
    .not_remote(
        "the server has no check route; a check runs on the repository's host, so run by check \
         there or inside the branch's harness",
    ),
    op(
        "cancel",
        &["cancel"],
        "cancel",
        "cancel",
        "cancel",
        Capability::Delegate,
        &[TARGET, JSON],
    ),
    op(
        "discard",
        &["discard"],
        "discard",
        "discard",
        "discard",
        Capability::Delegate,
        &[TARGET, Param::all("--reason", "reason", "reason"), JSON],
    ),
    op(
        "children",
        &["children"],
        "children",
        "children",
        "children",
        Capability::Own,
        &[
            Param::outside(
                "<BRANCH>",
                "inside a harness, children lists the harness's own branch's descendants",
            ),
            JSON,
        ],
    ),
    op(
        "wait",
        &["wait"],
        "wait",
        "wait_all",
        "wait_for",
        Capability::Own,
        &[
            Param::all("<BRANCHES>", "branches", "*branches"),
            Param::all("--any", "any", "any").without_python("Python's wait_any"),
            Param::cli(
                "--all",
                "the default: every surface waits for all unless told any",
            ),
            Param::all("--timeout", "timeout_seconds", "timeout"),
            JSON,
        ],
    ),
    op(
        "apply_graph",
        &["graph", "apply"],
        "apply_graph",
        "apply_graph",
        "apply_graph",
        Capability::Delegate,
        &[
            Param::all("--edits", "edits", "edits"),
            Param::all(
                "--expected-revision",
                "expected_revision",
                "expected_revision",
            ),
            Param::cli(
                "<FILE>",
                "reads the proposal from a file; the other surfaces take its edits directly",
            ),
            JSON,
            Param::outside(
                "--parent",
                "inside a harness the graph is always the harness's own branch's",
            ),
            Param::outside("--yes", PERMISSIONS),
            Param::outside("--ask", PERMISSIONS),
            Param::outside("--permissions", PERMISSIONS),
            Param::outside(
                "--allow-unapproved-tools",
                "a child keeps its parent's approval routing",
            ),
        ],
    ),
    op(
        "graph",
        &["graph", "show"],
        "graph",
        "graph",
        "graph",
        Capability::Own,
        &[TARGET, JSON],
    ),
    op(
        "publish_artifact",
        &["artifact", "publish"],
        "publish_artifact",
        "publish",
        "publish_artifact",
        Capability::Storage,
        &[
            Param::all("<FILE>", "path", "path"),
            Param::all("--name", "name", "name"),
            Param::all("--media-type", "media_type", "media_type"),
            Param::all("--label", "labels", "labels"),
            ACTING_BRANCH,
            JSON,
        ],
    ),
    op(
        "list_artifacts",
        &["artifact", "list"],
        "list_artifacts",
        "list_artifacts",
        "artifacts",
        Capability::Storage,
        &[ACTING_BRANCH, JSON],
    ),
    op(
        "get_artifact",
        &["artifact", "get"],
        "get_artifact",
        "get_artifact",
        "read_artifact",
        Capability::Storage,
        &[
            Param::all("<ID>", "id", "artifact_id"),
            Param::all("--out", "out", "out"),
            ACTING_BRANCH,
            JSON,
        ],
    ),
    op(
        "share_artifact",
        &["artifact", "share"],
        "share_artifact",
        "share_artifact",
        "share_artifact",
        Capability::Storage,
        &[
            Param::all("<ID>", "id", "artifact_id"),
            Param::all("--to", "to", "to"),
            ACTING_BRANCH,
            JSON,
        ],
    ),
    op(
        "export_artifacts",
        &["artifact", "export"],
        "",
        "",
        "export_artifacts",
        Capability::Storage,
        &[
            Param::cli("<IDS>", "only the CLI and Rust export bundles"),
            Param::cli("--out", "only the CLI and Rust export bundles"),
            ACTING_BRANCH,
            JSON,
        ],
    )
    .local_only("bundles are files on the repository's host; the server has no bundle route"),
    op(
        "import_artifacts",
        &["artifact", "import"],
        "",
        "",
        "import_artifacts",
        Capability::Storage,
        &[
            Param::cli("<FILE>", "only the CLI and Rust import bundles"),
            ACTING_BRANCH,
            JSON,
        ],
    )
    .local_only("bundles are files on the repository's host; the server has no bundle route"),
    op(
        "create_scratch",
        &["scratch", "create"],
        "create_scratch",
        "create_scratch",
        "create_scratch",
        Capability::Storage,
        &[Param::all("<NAME>", "name", "name"), ACTING_BRANCH, JSON],
    ),
    op(
        "list_scratch",
        &["scratch", "list"],
        "list_scratch",
        "list_scratch",
        "scratch_areas",
        Capability::Storage,
        &[ACTING_BRANCH, JSON],
    ),
    op(
        "share_scratch",
        &["scratch", "share"],
        "share_scratch",
        "share_scratch",
        "share_scratch",
        Capability::Storage,
        &[
            Param::all("<NAME>", "name", "name"),
            Param::all("--to", "to", "to"),
            ACTING_BRANCH,
            JSON,
        ],
    ),
    op(
        "lock_scratch",
        &["scratch", "lock"],
        "lock_scratch",
        "lock_scratch",
        "lock_scratch",
        Capability::Storage,
        &[Param::all("<NAME>", "name", "name"), ACTING_BRANCH, JSON],
    ),
    op(
        "unlock_scratch",
        &["scratch", "unlock"],
        "unlock_scratch",
        "unlock_scratch",
        "unlock_scratch",
        Capability::Storage,
        &[Param::all("<NAME>", "name", "name"), ACTING_BRANCH, JSON],
    ),
    op(
        "ask",
        &["ask"],
        "ask",
        "ask",
        "ask",
        Capability::Message,
        &[
            Param::all("<TEXT>", "text", "text"),
            Param::all("--wait", "wait_seconds", "wait"),
            ACT_AS,
            JSON,
        ],
    ),
    op(
        "report",
        &["report"],
        "report",
        "report",
        "report",
        Capability::Message,
        &[Param::all("<TEXT>", "text", "text"), ACT_AS, JSON],
    ),
    op(
        "escalate",
        &["escalate"],
        "escalate",
        "escalate",
        "escalate",
        Capability::Message,
        &[Param::all("<TEXT>", "text", "text"), ACT_AS, JSON],
    ),
    op(
        "answer",
        &["answer"],
        "answer",
        "answer",
        "answer",
        Capability::Delegate,
        &[
            Param::all("<MESSAGE_ID>", "message_id", "message_id"),
            Param::all("<TEXT>", "text", "text"),
            ACT_AS,
            JSON,
        ],
    ),
    op(
        "inbox",
        &["inbox"],
        "inbox",
        "inbox",
        "inbox",
        Capability::Message,
        &[Param::all("--unread", "unread", "unread"), ACT_AS, JSON],
    ),
    op(
        "approve_plan",
        &["plan", "approve"],
        "approve_plan",
        "approve_plan",
        "approve_plan",
        Capability::Delegate,
        &[
            TARGET,
            Param {
                cli: Some("--file"),
                tool: Some("edited"),
                python: Some("edited"),
                why: "",
                inside: None,
            },
            Param::outside(
                "--edit",
                "opens an editor, which only a person at a terminal has",
            ),
            Param::outside(
                "--editor",
                "opens an editor, which only a person at a terminal has",
            ),
        ],
    )
    .person_flags(
        "inside a harness a descendant keeps its own limits and policy; only a person runs a \
         turn in the foreground with new ones",
    ),
    op(
        "reject_plan",
        &["plan", "reject"],
        "reject_plan",
        "reject_plan",
        "reject_plan",
        Capability::Delegate,
        &[
            TARGET,
            Param::all("--reason", "reason", "reason"),
            Param::all("--replan", "replan", "replan"),
        ],
    )
    .person_flags(
        "inside a harness a descendant keeps its own limits and policy; only a person runs a \
         turn in the foreground with new ones",
    ),
    op(
        "answer_approval",
        &["approvals", "allow"],
        "answer_approval",
        "",
        "answer_approval",
        Capability::Delegate,
        &[
            Param::all("<ID>", "id", "id").without_python(
                "approvals reach a parent's inbox; Python answers them with by approvals",
            ),
            Param::all("--reason", "reason", "reason").without_python(
                "approvals reach a parent's inbox; Python answers them with by approvals",
            ),
            Param {
                cli: None,
                tool: Some("allow"),
                python: None,
                why: "the CLI's subcommand says it: by approvals allow, or by approvals deny",
                inside: None,
            },
            Param::cli(
                "--branch",
                "picks the oldest approval waiting on a branch; the tool names it by id",
            ),
        ],
    )
    .no_python("approvals reach a parent's inbox; Python answers them with by approvals"),
];

/// Every operation an MCP tool reaches.
pub fn tools() -> impl Iterator<Item = &'static Operation> {
    OPERATIONS.iter().filter(|o| o.tool.is_some())
}

/// The operation an MCP tool (or its alias) names.
pub fn by_tool(tool: &str) -> Option<&'static Operation> {
    let tool = match tool {
        // The SDK's own name for propose_integration.
        "integrate" => "propose_integration",
        other => other,
    };
    OPERATIONS.iter().find(|o| o.tool == Some(tool))
}

/// Whether `words` (a `by` command line after the program) runs an
/// operation a harness may run on its own branch: what
/// [`crate::Policy::allow_delegation_commands`] allows.
pub fn is_harness_command(words: &[String]) -> bool {
    let starts = |cli: &[&str]| {
        words.len() >= cli.len() && words.iter().zip(cli).all(|(word, cli)| word == cli)
    };
    OPERATIONS.iter().any(|operation| {
        operation.contexts.inside == Context::Yes
            && (starts(operation.cli) || operation.alias.is_some_and(starts))
    })
}

/// The operation named `name`.
pub fn by_name(name: &str) -> Option<&'static Operation> {
    OPERATIONS.iter().find(|o| o.name == name)
}

/// A limit's name in errors and statuses (`max_usd`) and the flag that
/// sets it, so a message can name both.
pub const LIMIT_FLAGS: &[(&str, &str)] = &[
    ("max_usd", "--budget-usd"),
    ("max_turns", "--max-turns"),
    ("max_duration", "--max-minutes"),
];

/// `max_usd (--budget-usd)`: a limit and the flag that sets it.
pub fn limit_text(limit: &str) -> String {
    match LIMIT_FLAGS.iter().find(|(name, _)| *name == limit) {
        Some((name, flag)) => format!("{name} ({flag})"),
        None => limit.to_owned(),
    }
}

/// Flags `by run` and `by spawn` both take, with the same meaning: what a
/// person starts a root with, a parent may give a child.
pub const SHARED_TASK_FLAGS: &[&str] = &[
    "--harness",
    "--name",
    "--base",
    "--check",
    "--budget-usd",
    "--max-turns",
    "--max-minutes",
    "--deny",
    "--plan",
    "--connector",
    "--model",
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn names_tools_and_functions_are_unique() {
        let mut names = BTreeSet::new();
        let mut tools = BTreeSet::new();
        let mut python = BTreeSet::new();
        for operation in OPERATIONS {
            assert!(names.insert(operation.name), "{}", operation.name);
            if let Some(tool) = operation.tool {
                assert!(tools.insert(tool), "{tool}");
            }
            if let Some(function) = operation.python {
                assert!(python.insert(function), "{function}");
            }
        }
    }

    #[test]
    fn every_gap_says_why() {
        for operation in OPERATIONS {
            let gap =
                operation.tool.is_none() || operation.python.is_none() || operation.rust.is_none();
            assert!(
                !gap || !operation.why.is_empty(),
                "{} lacks a surface and does not say why",
                operation.name
            );
            for param in operation.params {
                let gap = param.cli.is_none()
                    || (operation.tool.is_some() && param.tool.is_none())
                    || (operation.python.is_some() && param.python.is_none());
                assert!(
                    !gap || !param.why.is_empty(),
                    "{}'s {param:?} lacks a surface and does not say why",
                    operation.name
                );
            }
        }
    }

    /// A leaf's token reaches its own branch, its storage and its parent,
    /// never another branch's work.
    #[test]
    fn a_leaf_holds_every_capability_but_delegating() {
        for capability in [Capability::Own, Capability::Storage, Capability::Message] {
            assert!(capability.held(false));
        }
        assert!(!Capability::Delegate.held(false));
        assert!(Capability::Delegate.held(true));
        for name in [
            "spawn",
            "send",
            "steer",
            "integrate",
            "cancel",
            "discard",
            "answer",
        ] {
            assert_eq!(by_name(name).unwrap().capability, Capability::Delegate);
        }
    }

    fn source(path: &str) -> String {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        std::fs::read_to_string(root.join(path)).unwrap()
    }

    /// `def function(a, b: T = x, ...)`'s parameter names.
    fn python_parameters(module: &str, function: &str) -> Option<Vec<String>> {
        let start = module.find(&format!("\ndef {function}("))? + function.len() + 6;
        let end = start + module[start..].find(')')?;
        let mut names = Vec::new();
        let (mut depth, mut part) = (0, String::new());
        for c in module[start..end].chars().chain([',']) {
            match c {
                '[' => depth += 1,
                ']' => depth -= 1,
                ',' if depth == 0 => {
                    let name = part.split([':', '=']).next().unwrap_or_default().trim();
                    if !name.is_empty() {
                        names.push(name.to_owned());
                    }
                    part.clear();
                    continue;
                }
                _ => {}
            }
            part.push(c);
        }
        Some(names)
    }

    /// The Python module has each operation's function, taking exactly
    /// the keyword arguments the table names.
    #[test]
    fn python_functions_take_the_tables_arguments() {
        let module = source("sdk/python/branchyard.py");
        for operation in OPERATIONS {
            let Some(function) = operation.python else {
                continue;
            };
            let parameters = python_parameters(&module, function)
                .unwrap_or_else(|| panic!("branchyard.py has no {function}"));
            let expected: Vec<String> = operation
                .params
                .iter()
                .filter_map(|p| p.python)
                .map(str::to_owned)
                .collect();
            let mut sorted = parameters.clone();
            sorted.sort();
            let mut wanted = expected.clone();
            wanted.sort();
            assert_eq!(sorted, wanted, "branchyard.{function}'s parameters");
            assert!(
                module.contains(&format!("\"{function}\",")),
                "branchyard.py's __all__ lacks {function}"
            );
        }
    }

    #[test]
    fn the_python_reader_reads_signatures() {
        let module = "\ndef f(a: str, b: Optional[Dict[str, str]] = None,\n  c=1) -> X:\n";
        assert_eq!(python_parameters(module, "f").unwrap(), ["a", "b", "c"]);
        assert_eq!(python_parameters(module, "g"), None);
    }

    /// `Delegate` has each operation's method, and the engine dispatches
    /// each tool.
    #[test]
    fn every_method_exists_and_every_tool_is_dispatched() {
        let delegation = source("crates/branchyard/src/delegation.rs");
        for operation in OPERATIONS {
            if let Some(method) = operation.rust {
                assert!(
                    delegation.contains(&format!("    pub fn {method}(")),
                    "Delegate has no {method}"
                );
            }
        }
        let start = delegation.find("pub(crate) fn dispatch(").unwrap();
        let body = &delegation[start..start + delegation[start..].find("\n}\n").unwrap()];
        for tool in tools().filter_map(|o| o.tool) {
            assert!(
                body.contains(&format!("\"{tool}\"")),
                "dispatch lacks {tool}"
            );
        }
    }

    /// `by steer` is `by send --steer`, and a harness may run either.
    #[test]
    fn steer_has_its_own_command() {
        let steer = by_name("steer").unwrap();
        assert_eq!(steer.alias, Some(&["steer"][..]));
        let words = |line: &str| line.split(' ').map(str::to_owned).collect::<Vec<_>>();
        assert!(is_harness_command(&words("steer child go")));
        assert!(is_harness_command(&words("send child go --steer")));
        assert!(!is_harness_command(&words("steering child")));
    }

    #[test]
    fn limits_name_their_flags() {
        assert_eq!(limit_text("max_usd"), "max_usd (--budget-usd)");
        assert_eq!(limit_text("other"), "other");
        assert_eq!(by_tool("integrate").unwrap().name, "integrate");
    }
}
