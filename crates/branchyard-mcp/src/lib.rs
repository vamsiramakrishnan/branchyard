//! Branchyard's delegation tools as a stdio MCP server.
//!
//! The engine starts a harness with this server when the branch may
//! delegate (`TaskOptions::delegation`), passing `--root <repository>
//! --branch <name>` and the turn's token in `BRANCHYARD_DELEGATION`. The
//! server offers `spawn`, `inspect`, `events`, `send`,
//! `propose_integration`, `cancel` and `children`, and forwards each call
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
pub const TOOLS: [&str; 7] = [
    "spawn",
    "inspect",
    "events",
    "send",
    "propose_integration",
    "cancel",
    "children",
];

const INSTRUCTIONS: &str = "Branchyard runs you on a git branch. These tools let you \
delegate: spawn child branches with their own harness and budget, watch them with inspect \
and events, continue them with send, merge a finished child into your own branch with \
propose_integration (its check must pass), stop them with cancel, and list them with \
children. Children run in parallel; spawn returns once a child has started. You act only as your own branch and only \
on your descendants. inspect with no branch shows your remaining budget.";

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
    vec![
        Tool::new(
            "spawn",
            "Create a child branch and start a harness on it with a prompt. It starts from your \
             current work (your uncommitted changes are committed to your branch first) or from \
             base. Its budget must fit in what you have left. Returns the child's name once it \
             has started; it runs in parallel with you.",
            schema(json!({
                "type": "object",
                "properties": {
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
                },
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

/// Usage for the command line.
pub const USAGE: &str = "Usage: branchyard-mcp --root <repository> --branch <name>\n\
The delegation token is read from BRANCHYARD_DELEGATION. The engine starts this server \
for a harness; it works only while that branch's turn runs.\n";

/// Why the command line failed.
#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    /// Bad arguments or a missing token: exit 2.
    Usage(String),
    /// Serving failed.
    Serve(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Usage(message) | Failure::Serve(message) => f.write_str(message),
        }
    }
}

/// Run the command line: `--root <repository> --branch <name>`, token in
/// [`ENV_TOKEN`].
pub fn main_with_args(args: &[String]) -> Result<(), Failure> {
    let usage = Failure::Usage;
    let mut root = None;
    let mut branch = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_owned())),
            None => (arg.as_str(), None),
        };
        let mut value = || {
            inline
                .clone()
                .or_else(|| args.next().cloned())
                .ok_or_else(|| usage(format!("{flag} needs a value")))
        };
        match flag {
            "--root" => root = Some(PathBuf::from(value()?)),
            "--branch" => branch = Some(value()?),
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            other => return Err(usage(format!("unexpected argument '{other}'"))),
        }
    }
    let root = root.ok_or_else(|| usage("--root is required".into()))?;
    let branch = branch.ok_or_else(|| usage("--branch is required".into()))?;
    let token = std::env::var(ENV_TOKEN)
        .ok()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| usage(format!("{ENV_TOKEN} is not set")))?;
    serve_stdio(root, &branch, &token).map_err(|e| Failure::Serve(format!("serving MCP: {e}")))
}
