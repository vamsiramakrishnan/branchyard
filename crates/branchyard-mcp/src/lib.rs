//! Branchyard's delegation tools as a stdio MCP server.
//!
//! The engine starts a harness with this server when the branch may
//! delegate (`TaskOptions::delegation`), passing `--root <repository>
//! --branch <name>` and the turn's token in `BRANCHYARD_DELEGATION`. The
//! server offers `spawn`, `inspect`, `events`, `send`, `steer`,
//! `propose_integration`, `cancel`, `children`, `apply_graph` and `graph`
//! (dependencies between children; see `docs/graph.md`), and the artifact and
//! scratch-area tools (`publish_artifact`, `list_artifacts`,
//! `get_artifact`, `share_artifact`, `create_scratch`, `list_scratch`,
//! `share_scratch`, `lock_scratch`, `unlock_scratch`; see
//! `docs/storage.md`), and forwards each call
//! through [`branchyard::Delegate`] to the engine running that turn, where
//! children run on the engine's threads. `by spawn` and its siblings, and
//! the Python module, reach the same operations the same way; this server
//! is for harnesses that cannot run commands.
//!
//! Guarantees:
//!
//! - It acts only as the branch its token was issued to, and only on that
//!   branch's descendants; a `branch` argument names a target, never the
//!   caller. A token that matches no running turn is refused before the
//!   call leaves this process, and the engine checks it again.
//! - Refusals (envelope, budget, authority) are tool results with
//!   `isError: true` and a reason, so the model can adjust; malformed calls
//!   are JSON-RPC errors.
//!
//! Not guaranteed:
//!
//! - Use outside a running turn. With no engine to reach, every call fails.
//! - Protection from the harness it serves: in local mode that harness runs
//!   as your user and can read `.branchyard/`. See `docs/delegation.md`.
//!
//! It uses the official Rust MCP SDK (`rmcp`), server role and stdio
//! transport only, on a current-thread Tokio runtime.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use branchyard::{Delegate, ENV_TOKEN};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde_json::{json, Map, Value};

/// Tool names, in the order they are listed.
pub const TOOLS: [&str; 27] = [
    "spawn",
    "inspect",
    "events",
    "send",
    "steer",
    "propose_integration",
    "cancel",
    "children",
    "apply_graph",
    "graph",
    "publish_artifact",
    "list_artifacts",
    "get_artifact",
    "share_artifact",
    "create_scratch",
    "list_scratch",
    "share_scratch",
    "lock_scratch",
    "unlock_scratch",
    "ask",
    "report",
    "escalate",
    "answer",
    "inbox",
    "approve_plan",
    "reject_plan",
    "answer_approval",
];

const INSTRUCTIONS: &str = "Branchyard runs you on a git branch. These tools let you \
delegate: spawn child branches with their own harness and budget, watch them with inspect \
and events, continue them with send, add to a child's running turn with steer, merge a \
finished child into your own branch with \
propose_integration (its check must pass), stop them with cancel, and list them with \
children. Children run in parallel; spawn returns once a child has started. A child may \
depend on its siblings (depends_on): it waits, and starts once they have settled; apply_graph \
creates several children and dependencies at once, all or nothing, against the revision graph \
shows. You act only as your own branch and only \
on your descendants. inspect with no branch shows your remaining budget, and in a rig your seat and the seats you \
may spawn. You can also message: ask your parent a question (optionally waiting for its \
answer), report to it, escalate to it or, if your rig seat allows, further up; answer a \
descendant's message; and read your own inbox. A child spawned with plan: true writes a plan read-only and escalates it to you: approve_plan runs it (as proposed or edited), reject_plan ends it or, with replan, has it plan again. When a descendant's tool or connector call needs approval, the ask is escalated to you: answer_approval allows or denies it.";

fn schema(value: Value) -> Arc<Map<String, Value>> {
    match value {
        Value::Object(map) => Arc::new(map),
        _ => unreachable!("tool schemas are objects"),
    }
}

fn branch_property(what: &str) -> Value {
    json!({"type": "string", "description": what})
}

/// The tool definitions this server lists.
pub fn tools() -> Vec<Tool> {
    let read_only = |title: &str| {
        let mut annotations = ToolAnnotations::default();
        annotations.title = Some(title.to_owned());
        annotations.read_only_hint = Some(true);
        annotations.open_world_hint = Some(false);
        annotations
    };
    let mut inspect = Tool::new(
        "inspect",
        "A branch's status, candidate diffstat, cost, budget, children and last message. \
         Omit branch for your own, including what budget you have left to grant.",
        schema(json!({
            "type": "object",
            "properties": {"branch": branch_property("Your own branch or a descendant; defaults to your own")},
            "additionalProperties": false,
        })),
    );
    inspect.annotations = Some(read_only("Inspect a branch"));
    let mut events = Tool::new(
        "events",
        "Recorded activity for your branch or a descendant: prompts, harness events, permission \
         decisions, candidates and status changes. Without a cursor, returns the most recent; \
         pass next_cursor back to continue.",
        schema(json!({
            "type": "object",
            "properties": {
                "branch": branch_property("Your own branch or a descendant; defaults to your own"),
                "cursor": {"type": "integer", "minimum": 0, "description": "Event index to start from"},
                "limit": {"type": "integer", "minimum": 1, "maximum": 200, "description": "At most this many events (default 50)"},
            },
            "additionalProperties": false,
        })),
    );
    events.annotations = Some(read_only("Read a branch's events"));
    let mut children = Tool::new(
        "children",
        "Every branch you delegated to, directly or through your children, oldest first.",
        schema(json!({"type": "object", "properties": {}, "additionalProperties": false})),
    );
    children.annotations = Some(read_only("List your descendants"));
    let mut graph = Tool::new(
        "graph",
        "Your children (or a descendant's), the dependencies among them, each child's status, \
         and the graph revision apply_graph must be given.",
        schema(json!({
            "type": "object",
            "properties": {"branch": branch_property("Your own branch or a descendant; defaults to your own")},
            "additionalProperties": false,
        })),
    );
    graph.annotations = Some(read_only("Show a branch's graph"));
    let spawn_properties = json!({
        "prompt": {"type": "string", "description": "The child's task"},
        "harness": {"type": "string", "description": "Harness or profile ID, such as codex; defaults to yours and must be allowed by your envelope"},
        "name": {"type": "string", "description": "Branch name: lowercase [a-z0-9._-]; defaults to a slug of the prompt"},
        "base": {"type": "string", "description": "A git revision to start from instead of your current work"},
        "budget": {
            "type": "object",
            "properties": {
                "max_usd": {"type": "number", "exclusiveMinimum": 0, "description": "Cost limit; required when you have one"},
                "max_turns": {"type": "integer", "minimum": 1},
                "max_minutes": {"type": "number", "exclusiveMinimum": 0, "description": "Per turn"},
            },
            "additionalProperties": false,
        },
        "check": {"type": "array", "items": {"type": "string"}, "description": "Command argument vector that must pass before the child is merged; defaults to yours"},
        "max_depth": {"type": "integer", "minimum": 0, "description": "Levels the child may delegate below itself; at most one fewer than yours"},
        "max_children": {"type": "integer", "minimum": 0},
        "harnesses": {"type": "array", "items": {"type": "string"}, "description": "Harnesses the child may delegate to; each must be allowed to you"},
        "deny": {"type": "array", "items": {"type": "string"}, "description": "Tool names the child is denied outright; a trailing * matches a prefix"},
        "seat": {"type": "string", "description": "In a rig, the seat to fill; it sets the child's harness, limits, check and instructions. Required in a rig, and must be one of your seats (inspect shows them); refused outside one"},
        "depends_on": {"type": "array", "items": {"type": "string"}, "description": "Your other children this one waits for; it starts once they have settled, from your branch as it is then"},
        "after": {"type": "string", "enum": ["settled", "integrated"], "description": "settled (default): each dependency ended ready or no_changes, or was merged; integrated: you integrated it"},
        "bindings": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "scratch": {"type": "string", "description": "A scratch area you or an ancestor owns"},
                    "access": {"type": "string", "enum": ["read_only", "exclusive_write"], "description": "exclusive_write holds the area's writer lock for each of the child's turns"},
                },
                "required": ["scratch", "access"],
                "additionalProperties": false,
            },
        },
        "plan": {"type": "boolean", "description": "Plan first: the child's first turn is read-only and proposes a plan, escalated to your inbox; it changes nothing until you approve_plan"},
    });
    let mut spawn_edit = spawn_properties.clone();
    spawn_edit["kind"] = json!({"const": "spawn"});
    let edge = |kind: &str, after: bool| {
        let mut properties = json!({
            "kind": {"const": kind},
            "dependent": branch_property("One of your children that has not started"),
            "prerequisite": branch_property("Another of your children"),
        });
        if after {
            properties["after"] = json!({"type": "string", "enum": ["settled", "integrated"]});
        }
        json!({
            "type": "object",
            "properties": properties,
            "required": ["kind", "dependent", "prerequisite"],
            "additionalProperties": false,
        })
    };
    vec![
        Tool::new(
            "spawn",
            "Create a child branch and start a harness on it with a prompt. It starts from your \
             current work (your uncommitted changes are committed to your branch first) or from \
             base. Its budget must fit in what you have left. Returns the child's name once it \
             has started; it runs in parallel with you. With depends_on it is created waiting, \
             and starts once those children have settled.",
            schema(json!({
                "type": "object",
                "properties": spawn_properties,
                "required": ["prompt"],
                "additionalProperties": false,
            })),
        ),
        inspect,
        events,
        Tool::new(
            "send",
            "Send a follow-up prompt to a descendant that is not running a turn. Returns once \
             its turn has started.",
            schema(json!({
                "type": "object",
                "properties": {
                    "branch": branch_property("A descendant"),
                    "prompt": {"type": "string"},
                },
                "required": ["branch", "prompt"],
                "additionalProperties": false,
            })),
        ),
        Tool::new(
            "steer",
            "Add a message to a descendant's running turn without interrupting it; the \
             harness reads it at its next step, such as after the current tool call. Refused \
             when the descendant is not running a turn or its harness cannot take input \
             mid-turn. Returns whether it was delivered.",
            schema(json!({
                "type": "object",
                "properties": {
                    "branch": branch_property("A descendant that is running a turn"),
                    "text": {"type": "string"},
                },
                "required": ["branch", "text"],
                "additionalProperties": false,
            })),
        ),
        Tool::new(
            "propose_integration",
            "Merge a finished descendant's candidate into your own branch, after its check \
             passes on the exact merge. Your uncommitted changes are committed first, and your \
             working tree moves to the merge. Never touches the user's branches.",
            schema(json!({
                "type": "object",
                "properties": {"branch": branch_property("A descendant that is not running")},
                "required": ["branch"],
                "additionalProperties": false,
            })),
        ),
        Tool::new(
            "cancel",
            "Stop a descendant's running turn and every turn running below it. Each ends \
             interrupted; returns the branches that were stopped.",
            schema(json!({
                "type": "object",
                "properties": {"branch": branch_property("A descendant")},
                "required": ["branch"],
                "additionalProperties": false,
            })),
        ),
        children,
        Tool::new(
            "apply_graph",
            "Change your children's graph in one step, all or nothing: spawn children (each may \
             depend on others, including ones spawned in the same call) and add or remove \
             dependencies between children that have not started. expected_revision must be the \
             revision graph shows; if it moved on, nothing changes and you get stale_revision. \
             Children with nothing to wait for start at once.",
            schema(json!({
                "type": "object",
                "properties": {
                    "expected_revision": {"type": "integer", "minimum": 0, "description": "Your graph's revision, from graph"},
                    "edits": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": branchyard::MAX_EDITS,
                        "items": {"oneOf": [
                            {
                                "type": "object",
                                "properties": spawn_edit,
                                "required": ["kind", "prompt"],
                                "additionalProperties": false,
                            },
                            edge("add_dependency", true),
                            edge("remove_dependency", false),
                        ]},
                    },
                },
                "required": ["expected_revision", "edits"],
                "additionalProperties": false,
            })),
        ),
        graph,
        Tool::new(
            "publish_artifact",
            "Publish a file at a path in your worktree as a new immutable artifact of your \
             branch, content-addressed by its blake3 digest. Ancestors and descendants of your \
             branch can read it; a sibling needs an explicit share_artifact.",
            schema(json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path to the file, in your worktree"},
                    "name": {"type": "string", "description": "Defaults to the file's name"},
                    "media_type": {"type": "string"},
                    "labels": {"type": "object", "additionalProperties": {"type": "string"}},
                },
                "required": ["path"],
                "additionalProperties": false,
            })),
        ),
        {
            let mut t = Tool::new(
                "list_artifacts",
                "Every artifact you may read: what you published, what your ancestors or \
                 descendants published, and what was explicitly shared to you.",
                schema(json!({"type": "object", "properties": {}, "additionalProperties": false})),
            );
            t.annotations = Some(read_only("List readable artifacts"));
            t
        },
        Tool::new(
            "get_artifact",
            "Copy an artifact's bytes to a path in your worktree, checked against its recorded \
             digest, and return its provenance. Refused unless you may read it.",
            schema(json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "out": {"type": "string", "description": "Destination path, in your worktree"},
                },
                "required": ["id", "out"],
                "additionalProperties": false,
            })),
        ),
        Tool::new(
            "share_artifact",
            "Share an artifact you may read with another branch: the explicit grant a sibling \
             of its publisher needs.",
            schema(json!({
                "type": "object",
                "properties": {"id": {"type": "string"}, "to": branch_property("The branch to share with")},
                "required": ["id", "to"],
                "additionalProperties": false,
            })),
        ),
        Tool::new(
            "create_scratch",
            "Create a named shared scratch directory, owned by your branch, visible to your \
             ancestors and descendants at BRANCHYARD_SCRATCH_<NAME> (Microsandbox: a mount). One \
             writer at a time; see lock_scratch.",
            schema(json!({
                "type": "object",
                "properties": {"name": {"type": "string", "description": "Lowercase [a-z0-9-], starting with a letter"}},
                "required": ["name"],
                "additionalProperties": false,
            })),
        ),
        {
            let mut t = Tool::new(
                "list_scratch",
                "Every scratch area you may reach.",
                schema(json!({"type": "object", "properties": {}, "additionalProperties": false})),
            );
            t.annotations = Some(read_only("List reachable scratch areas"));
            t
        },
        Tool::new(
            "share_scratch",
            "Share a scratch area you may reach with another branch.",
            schema(json!({
                "type": "object",
                "properties": {"name": {"type": "string"}, "to": branch_property("The branch to share with")},
                "required": ["name", "to"],
                "additionalProperties": false,
            })),
        ),
        Tool::new(
            "lock_scratch",
            "Acquire a scratch area's writer lock for your branch: granted when free, re-granted \
             if you already hold it, or reclaimed once the current holder's turn has ended; \
             refused while another branch is still running with it held.",
            schema(json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"],
                "additionalProperties": false,
            })),
        ),
        Tool::new(
            "unlock_scratch",
            "Release a scratch area's writer lock if your branch holds it.",
            schema(json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"],
                "additionalProperties": false,
            })),
        ),
        Tool::new(
            "ask",
            "Ask your parent a question. Without wait_seconds, returns once it is sent. With \
             it, blocks for up to that long for an answer (in_reply_to your question); a wait \
             that passes with no answer yet is not an error, answer is just absent, ask inbox \
             or ask again.",
            schema(json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string"},
                    "wait_seconds": {"type": "number", "exclusiveMinimum": 0, "description": "Block up to this long for an answer"},
                },
                "required": ["text"],
                "additionalProperties": false,
            })),
        ),
        Tool::new(
            "report",
            "Report to your parent; no answer is expected.",
            schema(json!({
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"],
                "additionalProperties": false,
            })),
        ),
        Tool::new(
            "escalate",
            "Escalate to your parent, or, if your rig seat's escalates_to allows it, an \
             ancestor further up.",
            schema(json!({
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"],
                "additionalProperties": false,
            })),
        ),
        Tool::new(
            "answer",
            "Answer a message (usually a question) from one of your own descendants.",
            schema(json!({
                "type": "object",
                "properties": {
                    "message_id": {"type": "integer", "description": "The message's id, from inbox or the message you received"},
                    "text": {"type": "string"},
                },
                "required": ["message_id", "text"],
                "additionalProperties": false,
            })),
        ),
        {
            let mut inbox = Tool::new(
                "inbox",
                "Every message addressed to you, oldest first.",
                schema(json!({"type": "object", "properties": {}, "additionalProperties": false})),
            );
            inbox.annotations = Some(read_only("Read your inbox"));
            inbox
        },
        Tool::new(
            "approve_plan",
            "Approve a descendant's plan that awaits approval, as proposed or with edited in its              place, and start the turn that carries it out; returns once it has started.",
            schema(json!({
                "type": "object",
                "properties": {
                    "branch": branch_property("A descendant whose plan awaits approval"),
                    "edited": {"type": "string", "description": "The plan to approve instead of the proposed one"},
                },
                "required": ["branch"],
                "additionalProperties": false,
            })),
        ),
        Tool::new(
            "reject_plan",
            "Reject a descendant's plan: it ends, or with replan it plans again (read-only)              with your reason.",
            schema(json!({
                "type": "object",
                "properties": {
                    "branch": branch_property("A descendant whose plan awaits approval"),
                    "reason": {"type": "string"},
                    "replan": {"type": "boolean"},
                },
                "required": ["branch"],
                "additionalProperties": false,
            })),
        ),
        Tool::new(
            "answer_approval",
            "Allow or deny a descendant's approval ask: a tool or connector call its turn waits \
             on, or a staged effect it holds (docs/effects.md).",
            schema(json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "The approval's id, or the end of it"},
                    "allow": {"type": "boolean"},
                    "reason": {"type": "string"},
                },
                "required": ["id", "allow"],
                "additionalProperties": false,
            })),
        ),
    ]
}

/// One branch's tools over MCP.
#[derive(Clone)]
pub struct Server {
    root: PathBuf,
    branch: String,
    token: String,
    /// Found on the first call: the turn may not have registered yet when
    /// the harness starts this server.
    delegate: Arc<Mutex<Option<Delegate>>>,
}

impl Server {
    /// Serve as the branch whose running turn under `root` holds `token`;
    /// `branch` must be that branch.
    pub fn new(root: PathBuf, branch: &str, token: &str) -> Server {
        Server {
            root,
            branch: branch.to_owned(),
            token: token.to_owned(),
            delegate: Arc::default(),
        }
    }

    fn call(&self, tool: &str, arguments: Value) -> Result<Value, branchyard::Error> {
        let mut delegate = self.delegate.lock().unwrap_or_else(|e| e.into_inner());
        if delegate.is_none() {
            let found = Delegate::connect(&self.root, &self.token)?;
            if found.branch() != self.branch {
                return Err(branchyard::Error::Denied(format!(
                    "this delegation token was issued to {}, not {}",
                    found.branch(),
                    self.branch
                )));
            }
            *delegate = Some(found);
        }
        let result = delegate
            .as_ref()
            .expect("found above")
            .call(tool, arguments);
        if result.as_ref().is_err_and(|e| e.kind() == "state") {
            // A transport failure: find the engine again next time.
            *delegate = None;
        }
        result
    }
}

impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("branchyard", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let tool = request.name.to_string();
        if !TOOLS.contains(&tool.as_str()) {
            return Err(ErrorData::invalid_params(
                Cow::Owned(format!("no tool named {tool}")),
                None,
            ));
        }
        let arguments = request.arguments.map(Value::Object).unwrap_or(json!({}));
        let server = self.clone();
        // A call can run a merge check or wait on git; keep it off the
        // runtime's only thread.
        let result = tokio::task::spawn_blocking(move || server.call(&tool, arguments))
            .await
            .map_err(|e| ErrorData::internal_error(Cow::Owned(e.to_string()), None))?;
        Ok(match result {
            Ok(value) => {
                let text = serde_json::to_string_pretty(&value).unwrap_or_default();
                let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
                if value.is_object() {
                    result.structured_content = Some(value);
                }
                result
            }
            Err(error) => CallToolResult::error(vec![ContentBlock::text(error.to_string())]),
        }
        .into())
    }
}

/// Serve `branch`'s tools on stdin and stdout until the client closes them.
pub fn serve_stdio(root: PathBuf, branch: &str, token: &str) -> std::io::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    let server = Server::new(root, branch, token);
    runtime.block_on(async move {
        let running = server
            .serve(rmcp::transport::stdio())
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        running
            .waiting()
            .await
            .map(|_| ())
            .map_err(|e| std::io::Error::other(e.to_string()))
    })
}

/// The command line: `--root <repository> --branch <name>`.
#[derive(clap::Parser, Debug)]
#[command(
    name = "branchyard-mcp",
    version,
    about = "Branchyard's delegation tools for one branch, over MCP on stdio.",
    after_help = "The delegation token is read from BRANCHYARD_DELEGATION. The engine starts \
                  this server for a harness; it works only while that branch's turn runs."
)]
struct Cli {
    /// The repository root
    #[arg(long, value_name = "DIR")]
    root: PathBuf,
    /// The branch whose turn this server serves
    #[arg(long, value_name = "NAME")]
    branch: String,
}

/// Why the command line failed.
#[derive(Debug)]
pub enum Failure {
    /// Bad arguments: clap's error, printed as clap formats it, exit 2
    /// (or 0 for `--help` and `--version`).
    Clap(clap::Error),
    /// A missing token: exit 2.
    Usage(String),
    /// Serving failed.
    Serve(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Clap(error) => write!(f, "{}", error.to_string().trim_end()),
            Failure::Usage(message) | Failure::Serve(message) => f.write_str(message),
        }
    }
}

/// Run the command line (after the program name): `--root <repository>
/// --branch <name>`, token in [`ENV_TOKEN`].
pub fn main_with_args(args: &[String]) -> Result<(), Failure> {
    use clap::Parser;
    let program = std::iter::once("branchyard-mcp".to_owned());
    let cli = Cli::try_parse_from(program.chain(args.iter().cloned())).map_err(Failure::Clap)?;
    serve_branch(cli.root, &cli.branch)
}

/// Serve `branch`'s delegation tools on stdio, with the token in
/// [`ENV_TOKEN`]; `by mcp --root --branch` calls this directly.
pub fn serve_branch(root: PathBuf, branch: &str) -> Result<(), Failure> {
    let token = std::env::var(ENV_TOKEN)
        .ok()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| Failure::Usage(format!("{ENV_TOKEN} is not set")))?;
    serve_stdio(root, branch, &token).map_err(|e| Failure::Serve(format!("serving MCP: {e}")))
}
