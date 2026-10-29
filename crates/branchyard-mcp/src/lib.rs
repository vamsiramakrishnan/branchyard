//! Branchyard's delegation tools as a stdio MCP server.
//!
//! The engine starts a harness with this server when the branch may
//! delegate (`TaskOptions::delegation`), passing `--root <repository>
//! --branch <name>` and the turn's token in `BRANCHYARD_DELEGATION`. The
//! server offers `spawn`, `inspect`, `events`, `send`, `steer`,
//! `propose_integration`, `cancel`, `discard`, `children`, `wait`, `apply_graph` and `graph`
//! (dependencies between children; see `docs/graph.md`), and the artifact and
//! scratch-area tools (`publish_artifact`, `list_artifacts`,
//! `get_artifact`, `share_artifact`, `create_scratch`, `list_scratch`,
//! `share_scratch`, `lock_scratch`, `unlock_scratch`; see
//! `docs/storage.md`), and forwards each call
//! through [`branchyard::Delegate`] to the engine running that turn, where
//! children run on the engine's threads. `by spawn` and its siblings, and
//! the Python module, reach the same operations the same way; this server
//! is for harnesses that cannot run commands. For a harness in a sandbox,
//! which cannot start it, the engine runs it on the host, one process per
//! MCP session, behind an HTTP endpoint of the turn's
//! (`docs/delegation.md#sandboxed-harnesses`).
//!
//! Guarantees:
//!
//! - It acts only as the branch its token was issued to, and only on that
//!   branch's descendants; a `branch` argument names a target, never the
//!   caller. A token that matches no running turn is refused before the
//!   call leaves this process, and the engine checks it again.
//! - Refusals (envelope, budget, authority) are tool results with
//!   `isError: true` and a reason, so the model can adjust; malformed calls
//!   are JSON-RPC errors. An error that carries more than its message,
//!   such as a failed check naming every check of an integration, also has
//!   it as structured content, `{"error": {kind, message, detail}}`, as
//!   `by --json` prints it.
//!
//! Not guaranteed:
//!
//! - Use outside a running turn. With no engine to reach, every call fails.
//! - Protection from the harness it serves: in local mode that harness runs
//!   as your user and can read `.branchyard/`. See `docs/delegation.md`.
//!
//! It uses the official Rust MCP SDK (`rmcp`), server role and stdio
//! transport only, on a current-thread Tokio runtime. Its tools and their
//! arguments are checked against [`branchyard::operations`] by this
//! crate's tests.
#![warn(missing_docs)]

use branchyard_support::LockExt as _;
use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use branchyard::{Delegate, ENV_TOKEN};
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
    Implementation, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig,
    Tool, ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde_json::{json, Map, Value};

/// Tool names, in the order they are listed: the MCP tools of
/// [`branchyard::operations::OPERATIONS`].
pub fn tool_names() -> Vec<&'static str> {
    branchyard::operations::tools()
        .filter_map(|operation| operation.tool)
        .collect()
}

const INSTRUCTIONS: &str = "Branchyard runs you on a git branch. These tools let you \
delegate. The shortest way: call delegate once with a plan (each task as spawn takes it, with \
the paths it owns, and a brief they all share), then end your turn; you are woken with a summary \
when the children have settled. Otherwise: spawn child branches with their own harness and budget, watch them with inspect \
and events, continue them with send, add to a child's running turn with steer, merge a \
finished child into your own branch with \
propose_integration (its check must pass; give several in branches to merge them together and \
check the result once), run a branch's check on its current work with check (run it on \
yourself before you finish), stop them with cancel, set a settled one aside with discard (it is never \
run again), and list them with children. Children run in parallel; spawn returns once a child has \
started. To wait for them, call wait, or end your turn: when every child has settled, Branchyard \
starts your next turn with a summary of what each did. A child may \
depend on its siblings (depends_on): it waits, and starts once they have settled; apply_graph \
creates several children and dependencies at once, all or nothing, against the revision graph \
shows. You act only as your own branch and only \
on your descendants. A branch that may not delegate (its envelope's max_depth is 0) still has \
inspect, check, the artifact and scratch tools, and ask, report, escalate, report_status and inbox; the other tools \
refuse it. inspect with no branch shows your remaining budget, and in a rig your seat and the seats you \
may spawn. You can also message: ask your parent a question (optionally waiting for its \
answer), report to it (with a result, a JSON value, when your parent gave you a result_contract in inspect: it is checked against the contract's schema, and it is what your parent reads back), escalate to it or, if your rig seat allows, further up; answer a \
descendant's message; state your own status in a line with report_status (working, idle, blocked, complete or failure; blocked and failure also reach your parent's inbox), which is a claim shown beside the status Branchyard derives, not a message; and read your own inbox. To read a child's answer as JSON, give spawn a result contract ({schema, required}): the child reports a result matching it, and you read it as the result of inspect or of each settled child in wait. Each harness keeps its native planning, subagents and internal task graph; Branchyard depth limits count Branchyard branches only. For native final JSON without requiring the reporting tool, use a result contract with purpose: deliverable and required: true; a valid current-turn answer can complete without a Git diff. A rig seat can fix owned paths and a result contract: children may narrow ownership and may not replace the contract. Use handoff to freeze a settled descendant result and selected artifact IDs for another descendant; it waits for that recipient's next turn without steering or starting one. An external lead can use operator CLI/SDK calls without adopting or replacing its native session. A child spawned with plan: true writes a plan read-only and escalates it to you: approve_plan runs it (as proposed or edited), reject_plan ends it or, with replan, has it plan again. When a descendant's tool or connector call needs approval, the ask is escalated to you: answer_approval allows or denies it. Substrate setup is an operator action outside a harness: \
by substrate setup prepares the saved template; by substrate check --smoke verifies bridge execution \
and cleanup without a model. by up and by doctor report the saved connection and remote credential readiness. \
An operator can run by setup --guided to select a repository and compute backend. \
by setup --up --json includes readiness in one report; --directory PATH selects the repository. \
An operator can use by compute gce --project PROJECT --zone ZONE --name NAME --tunnel-through-iap \
to review an offline JSON plan; --apply provisions the VM and its dedicated IAP TCP 22 firewall rule. \
Add --internal-ip to omit an external address. This operator command refuses harness sessions and --remote.";

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
        "A branch's status, candidate diffstat, cost, budget, children and last message, and \
         its result: the JSON it reported with report, beside the result_contract it was \
         spawned with. Omit branch for your own, including what budget you have left to \
         grant, what the cost limits above you leave you (tree_remaining_usd), and the \
         contract you must answer in.",
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
    let mut advise = Tool::new(
        "advise",
        "What to know before picking a harness for a subtask: the candidates (your task's fleet \
         entry for its kind, or the harnesses you may run), each with whether it can run here, \
         its recorded outcomes for this kind and the success rate the router samples around, \
         the router's own pick, your budget left, and notes (no outcomes yet, risk words). Call \
         it before spawn, choose with it, and pass your reason as spawn's why.",
        schema(json!({
            "type": "object",
            "properties": {
                "prompt": {"type": "string", "description": "The subtask, as you would give it to spawn"},
                "kind": {"type": "string", "description": "Its kind instead of the classifier's: bugfix, feature, refactor, review, research, docs, migration, tests or other"},
            },
            "required": ["prompt"],
            "additionalProperties": false,
        })),
    );
    advise.annotations = Some(read_only("Advise on a harness for a subtask"));
    let mut check = Tool::new(
        "check",
        "Run a branch's check (its own, or the one it inherited) on its current work the way \
         propose_integration would, and integrate nothing: its files as they are, uncommitted \
         ones included, merged into its parent's branch in a private worktree. Omit branch to \
         check your own work before you finish: what fails here fails your integration. \
         Returns the outcome (passed, failed, timed_out, not_started; not_run when there is no \
         check or the merge conflicts, with conflicts naming the files) and the output's tail.",
        schema(json!({
            "type": "object",
            "properties": {"branch": branch_property("Your own branch or a descendant; defaults to your own")},
            "additionalProperties": false,
        })),
    );
    check.annotations = Some(read_only("Run a branch's check"));
    let spawn_properties = json!({
        "prompt": {"type": "string", "description": "The child's task"},
        "harness": {"type": "string", "description": "Harness or profile ID, such as codex; defaults to yours and must be allowed by your envelope. auto: the router places the child among those you may delegate to that can run here, by their recorded outcomes for the task, preferring one with a free slot"},
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
        "budget_usd": {"type": "number", "exclusiveMinimum": 0, "description": "budget.max_usd, flat; give these or budget, not both"},
        "max_turns": {"type": "integer", "minimum": 1, "description": "budget.max_turns, flat"},
        "max_minutes": {"type": "number", "exclusiveMinimum": 0, "description": "budget.max_minutes, flat"},
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
        "connectors": {"type": "array", "items": {"type": "string"}, "description": "Connector grants, as --connector takes them (github:read, 'github:write:issues.*'); narrowed to yours. Unset: yours, or the seat's"},
        "plan": {"type": "boolean", "description": "Plan first: the child's first turn is read-only and proposes a plan, escalated to your inbox; it changes nothing until you approve_plan"},
        "model": {"type": "string", "description": "The model the child's harness runs, or a size alias (small, medium, large, extra-large) where it defines one; a cheaper one suits mechanical work. Unset: the seat's, or yours. Refused for a harness that cannot choose one"},
        "why": {"type": "string", "description": "Why this harness and model for this subtask, in a few words: recorded on the child and learned from by the outcome store beside how it turns out. Ask advise first"},
        "owns": {"type": "array", "items": {"type": "string"}, "description": "The paths the child owns: files, directories (and all below them) or globs such as tests/parser_*.rs, relative to the repository. It is told them every turn, and its changes anywhere else are refused when you integrate it. Give siblings disjoint paths and they cannot conflict"},
        "land": {"type": "boolean", "description": "Leave the child to your engine to land: once its turn ends ready while you wait on your children, it is integrated into your branch, its check on the merge included; one that conflicts, fails its check or strays outside owns is sent a turn to fix that. Your next turn says what landed and what did not, so you integrate nothing yourself"},
        "review": {
            "type": "object",
            "description": "A reviewer that reads the child's work each time it is ready to land, before it lands: a harness you may delegate to, ideally another vendor's than the child's. It approves the work or lists findings, which are sent to the child as a turn to fix its work. Needs land",
            "properties": {
                "harness": {"type": "string", "description": "The reviewer's harness or profile ID, such as codex"},
                "model": {"type": "string", "description": "The model it runs; unset, its harness's default"},
                "rubric": {"type": "string", "description": "What to look for, added to its prompt"},
                "budget_usd": {"type": "number", "exclusiveMinimum": 0, "description": "Each review's cost limit, from your budget; required when you have a cost limit"},
            },
            "required": ["harness"],
            "additionalProperties": false,
        },
        "result": {
            "type": "object",
            "description": "The answer schema, visible to the child as result_contract. Report is the default: the child uses report. Deliverable requires a current-turn result, accepts native final JSON after successful completion, and can complete without Git changes. Inspect and wait expose the accepted result.",
            "properties": {
                "schema": {"type": "object", "description": "A JSON Schema using only type (object, array, string, number, integer, boolean, null, or a list), properties, required, additionalProperties, items, enum, const, minimum, maximum, exclusiveMinimum, exclusiveMaximum, minLength, maxLength, pattern, minItems, maxItems, uniqueItems, anyOf, oneOf, and annotations such as title and description; any other keyword, such as $ref, allOf or format, is refused"},
                "required": {"type": "boolean", "description": "true: a report without a result is refused, and you get a warning in your log when the child ends its turn without one. Default false"},
                "purpose": {"type": "string", "enum": ["report", "deliverable"], "description": "report keeps metadata reporting; deliverable requires required=true and accepts a native final JSON answer without a Git change"},
            },
            "required": ["schema"],
            "additionalProperties": false,
        },
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
             base. Its budget must fit in what you have left and in what the cost limits above you \
             leave their trees (inspect shows both); it is refused by name when it does not, and \
             once your tree has spent your limit. A child is stopped over budget when the tree \
             under a limit above it has spent that limit. Returns the child's name once it \
             has started; it runs in parallel with you. With depends_on it is created waiting, \
             and starts once those children have settled. With result, the child must answer \
             in the shape you give, and you read its answer back as JSON from inspect or wait.",
            schema(json!({
                "type": "object",
                "properties": spawn_properties,
                "required": ["prompt"],
                "additionalProperties": false,
            })),
        ),
        Tool::new(
            "delegate",
            "Hand your engine a plan: several children at once, each task as spawn takes it, \
             with the brief they share. They are created all or none; those that wait for \
             nothing (depends_on may name another task) start at once. The brief (the goal, \
             the conventions, the interfaces between their parts) is added to your shared brief \
             before any of them starts, and every child you delegate to is given it each turn. \
             Give each task the paths it owns, disjoint from its siblings', so they cannot \
             conflict. With land, your engine integrates each child into your branch when it is \
             ready, and sends one that cannot land a turn to fix it. Then end your turn: you are \
             started again with a summary of each child when they have all settled.",
            schema(json!({
                "type": "object",
                "properties": {
                    "brief": {"type": "string", "description": "What every child shares: the goal, the conventions, the interfaces between their parts. Added to your shared brief; add later decisions with brief"},
                    "tasks": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": spawn_properties,
                            "required": ["prompt"],
                            "additionalProperties": false,
                        },
                        "description": "The subtasks, each as spawn takes a child; give each a name when another depends on it",
                    },
                    "land": {"type": "boolean", "description": "Leave every task to your engine to land, as spawn's land does: each is integrated into your branch when it is ready, and your next turn says what landed"},
                    "review": spawn_properties["review"].clone(),
                },
                "required": ["tasks"],
                "additionalProperties": false,
            })),
        ),
        Tool::new(
            "brief",
            "Your shared brief: what every child you delegate to is given each turn, oldest \
             entry first. With text, add a new entry first, such as an interface you settled: \
             children running now are sent it, the rest read it at their next turn.",
            schema(json!({
                "type": "object",
                "properties": {"text": {"type": "string", "description": "A new entry to add; omit to read the brief"}},
                "additionalProperties": false,
            })),
        ),
        inspect,
        events,
        Tool::new(
            "send",
            "Send a follow-up prompt to a descendant that is not running a turn. Returns once \
             its turn has started. With retry and no prompt, submit again the prompt of its \
             last turn that was cut off when the engine running it stopped.",
            schema(json!({
                "type": "object",
                "properties": {
                    "branch": branch_property("A descendant"),
                    "prompt": {"type": "string"},
                    "retry": {"type": "boolean"},
                },
                "required": ["branch"],
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
            "Integrate (merge now): merge a finished descendant's candidate into your own \
             branch, after its check passes on the exact merge. With branches, merge several together, in order, all or \
             none, and run their check once on the result: for children that share a test \
             suite none passes alone. Your uncommitted changes are committed first, and your \
             working tree moves to the merge. A candidate your branch already contains is \
             recorded as merged (already: true), not refused. Never touches the user's branches.",
            schema(json!({
                "type": "object",
                "properties": {
                    "branch": branch_property("A descendant that is not running"),
                    "branches": {
                        "type": "array",
                        "items": {"type": "string"},
                        "minItems": 1,
                        "description": "Instead of branch (give exactly one of the two): several descendants, integrated together",
                    },
                },
                // Exactly one of the two, checked when called: a tool's
                // input schema may not have oneOf, anyOf or allOf at its top
                // level, and Claude Code drops every tool of a server that
                // lists one.
                "additionalProperties": false,
            })),
        ),
        check,
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
        Tool::new(
            "discard",
            "Set a settled descendant aside: it ends discarded with your reason, runs no more \
             turns, is never integrated, keeps its record and cost, and no longer counts \
             against your max_children. Refused while it runs (cancel it first). Returns it as \
             inspect shows it.",
            schema(json!({
                "type": "object",
                "properties": {
                    "branch": branch_property("A descendant that is not running"),
                    "reason": {"type": "string", "description": "Why, recorded with it"},
                },
                "required": ["branch"],
                "additionalProperties": false,
            })),
        ),
        children,
        {
            let mut t = Tool::new(
                "wait",
                "Block until descendants settle (stop running, or waiting for prerequisites or \
                 on their own children): all of them, or with any the first. Without branches, \
                 your children that are still running. Returns each settled one's inspection and \
                 those still pending (a settled child's result is in its inspection); timed_out \
                 is true when timeout_seconds passed first. You \
                 may instead end your turn: you are woken when every child has settled.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "branches": {"type": "array", "items": {"type": "string"}, "description": "Descendants to wait for; defaults to your children still running"},
                        "any": {"type": "boolean", "description": "Return when the first of them settles"},
                        "timeout_seconds": {"type": "number", "minimum": 0},
                    },
                    "additionalProperties": false,
                })),
            );
            t.annotations = Some(read_only("Wait for descendants"));
            t
        },
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
        advise,
        Tool::new(
            "publish_artifact",
            "Publish a file at a path in your worktree as a new immutable artifact of your \
             branch, content-addressed by its digest (the blake3 hash of its bytes, hex). \
             Ancestors and descendants of your branch can read it; a sibling needs an explicit \
             share_artifact.",
            schema(json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path to the file, in your worktree"},
                    "name": {"type": "string", "description": "Defaults to the file's name"},
                    "media_type": {"type": "string", "description": "Recorded with it, such as application/json; default application/octet-stream"},
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
            "handoff",
            "Relay a settled descendant's current result and selected artifacts to another descendant. The frozen handoff waits for the recipient's next turn; it never steers or starts one.",
            schema(json!({
                "type": "object",
                "properties": {
                    "from": branch_property("The settled producer, under your authority"),
                    "to": branch_property("The recipient, under your authority"),
                    "artifacts": {"type": "array", "items": {"type": "string"}, "description": "Only these immutable artifact IDs are shared"},
                    "note": {"type": "string", "description": "Context for the recipient"},
                    "expected": {
                        "type": "object",
                        "properties": {
                            "incarnation": {"type": "integer"},
                            "turn": {"type": "integer", "minimum": 0},
                            "result_event": {"type": "integer", "minimum": 0}
                        },
                        "required": ["incarnation", "turn", "result_event"],
                        "additionalProperties": false
                    }
                },
                "required": ["from", "to"],
                "additionalProperties": false
            })),
        ),
        Tool::new(
            "ask",
            "Ask your parent a question. Without wait_seconds, returns once it is sent. With \
             it, blocks for up to that long for an answer (in_reply_to your question); a wait \
             that passes with no answer yet is not an error, answer is just absent: read your \
             inbox, or ask again.",
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
            "Report to your parent; no answer is expected. Give result, a JSON value, to \
             answer in the shape your parent asked for: if your inspect shows a \
             result_contract, result is checked against its schema (a mismatch is refused, \
             naming the path and the rule, and nothing is recorded), and when the contract is \
             required, a report without one is refused. The latest result is what your parent \
             reads as your inspect's result.",
            schema(json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string"},
                    "result": {"description": "Your answer as a JSON value (an object, array, string, number or boolean), matching result_contract when you have one. null counts as none, unless your result_contract's schema accepts null"},
                },
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
            "report_status",
            "State your own status in one line, beside the one Branchyard derives: what you \
             are doing now (working), that you are waiting (idle), that you cannot go on \
             without something you name (blocked), that the task is done (complete), or that \
             you gave up (failure). Your parent and the person watching see it next to your \
             status. Blocked and failure are also escalated to your parent's inbox. It is a \
             claim, not a result: it changes nothing about your branch, and report is still \
             how you send your parent a message. Give ttl_seconds when the claim holds only \
             for a while: once it passes, the report is shown as stale. A report also goes \
             stale when a later turn of yours finishes without a new one, or your branch \
             ends.",
            schema(json!({
                "type": "object",
                "properties": {
                    "state": {
                        "type": "string",
                        "enum": branchyard::ReportedState::NAMES,
                        "description": "What you are doing: working, idle, blocked, complete or failure",
                    },
                    "summary": {"type": "string", "maxLength": branchyard::reported::SUMMARY_MAX, "description": "One line, at most 280 characters"},
                    "link": {"type": "string", "description": "Optional http(s) URL with more, such as a pull request or an issue"},
                    "ttl_seconds": {
                        "type": "integer",
                        "minimum": branchyard::reported::TTL_MIN_SECONDS,
                        "maximum": branchyard::reported::TTL_MAX_SECONDS,
                        "description": "Optional: how long the claim holds, in whole seconds (1 to 604800, 7 days); after that it is shown as stale",
                    },
                },
                "required": ["state", "summary"],
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
                "Every message addressed to you, oldest first; with unread, only those not yet \
                 delivered to a turn.",
                schema(json!({
                    "type": "object",
                    "properties": {"unread": {"type": "boolean"}},
                    "additionalProperties": false,
                })),
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

    #[allow(clippy::expect_used)] // ratchet: branchyard-mcp
    fn call(&self, tool: &str, arguments: Value) -> Result<Value, branchyard::Error> {
        let mut delegate = self.delegate.lock_recovering("delegate");
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
        // Protocol 2026-07-28 requires the cache hints on a list, and
        // Claude Code 2.1.293 negotiates it and refuses a list without
        // them: the server shows as connected, with no tools. The tools
        // are this branch's alone and may change with the binary.
        Ok(ListToolsResult::with_all_items(tools())
            .with_ttl_ms(0)
            .with_cache_scope(CacheScope::Private))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let tool = request.name.to_string();
        if !tool_names().contains(&tool.as_str()) {
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
            Err(error) => tool_error(&error),
        }
        .into())
    }
}

/// A failed call as the model reads it: the error's message, and its
/// detail, when it has one, as structured content.
fn tool_error(error: &branchyard::Error) -> CallToolResult {
    // The model reads no `kind` in the text, so a refusal says it is one.
    let text = match error {
        branchyard::Error::Denied(why) => format!("denied: {why}"),
        // A refusal the broker returned keeps its word too.
        branchyard::Error::Remote { kind, message, .. } if kind == "denied" => {
            format!("denied: {message}")
        }
        other => other.to_string(),
    };
    let mut result = CallToolResult::error(vec![ContentBlock::text(text)]);
    if let Some(detail) = error.detail() {
        result.structured_content = Some(json!({
            "error": {"kind": error.kind(), "message": error.to_string(), "detail": detail}
        }));
    }
    result
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

#[allow(clippy::unwrap_used, clippy::panic)] // tests: a panic is the failure report
#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn description(name: &str) -> String {
        let tool = tools().into_iter().find(|t| t.name == name).unwrap();
        tool.description.unwrap().to_string()
    }

    /// A model reads these two before it chooses: `ask` must not read as
    /// a call to make with the text "inbox", and `propose_integration`
    /// merges at once, whatever its name suggests.
    #[test]
    fn the_descriptions_of_ask_and_propose_integration_say_what_happens() {
        let ask = description("ask");
        assert!(
            ask.ends_with("answer is just absent: read your inbox, or ask again."),
            "{ask}"
        );
        assert!(!ask.contains("ask inbox"), "{ask}");
        let integrate = description("propose_integration");
        assert!(
            integrate.starts_with("Integrate (merge now): merge a finished descendant's"),
            "{integrate}"
        );
    }

    /// An error with a detail, as the broker returns a failed integration
    /// check, gives it as structured content beside its text; one without
    /// gives text alone.
    #[test]
    fn an_error_with_a_detail_has_it_as_structured_content() {
        let detail = json!({"checks": [
            {"check": ["make", "test"], "branches": ["b"], "outcome": "failed"}
        ]});
        let failed = tool_error(&branchyard::Error::Remote {
            kind: "check_failed".into(),
            message: "check failed:\nboom".into(),
            detail: Some(Box::new(detail.clone())),
        });
        assert_eq!(failed.is_error, Some(true));
        let structured = failed.structured_content.unwrap();
        assert_eq!(structured["error"]["kind"], "check_failed");
        assert_eq!(structured["error"]["message"], "check failed:\nboom");
        assert_eq!(structured["error"]["detail"], detail);
        let plain = tool_error(&branchyard::Error::Denied("no".into()));
        assert_eq!(plain.is_error, Some(true));
        assert!(plain.structured_content.is_none());
    }

    /// A check of an integration that could not start, run in this
    /// process, gives its checks as structured content too.
    #[test]
    fn a_check_that_could_not_start_has_its_checks_as_structured_content() {
        let not_started = tool_error(&branchyard::Error::CheckNotStarted {
            reason: "no such program".into(),
            checks: vec![branchyard::IntegrationCheck {
                check: vec!["nope".into()],
                branches: vec!["b".into()],
                outcome: branchyard::CheckVerdict::NotStarted,
            }],
        });
        let structured = not_started.structured_content.unwrap();
        assert_eq!(structured["error"]["kind"], "check_not_started");
        assert_eq!(
            structured["error"]["detail"]["checks"],
            json!([{"check": ["nope"], "branches": ["b"], "outcome": "not_started"}])
        );
    }

    /// The server lists exactly the table's tools, in its order, each with
    /// exactly the arguments the table names; a tool or an argument added
    /// here but not there (or there but not here) fails.
    /// Model APIs refuse a tool whose input schema combines alternatives
    /// at its top level, and Claude Code then lists none of this server's
    /// tools, silently. Every schema is a plain object.
    #[test]
    fn no_schema_has_top_level_alternatives() {
        for tool in tools() {
            let schema = tool.input_schema.as_ref();
            assert_eq!(schema.get("type"), Some(&json!("object")), "{}", tool.name);
            for key in ["oneOf", "anyOf", "allOf", "not", "enum", "if"] {
                assert!(
                    !schema.contains_key(key),
                    "{} has top-level {key}",
                    tool.name
                );
            }
        }
    }

    #[test]
    fn the_tools_are_the_operations_table() {
        let listed = tools();
        let names: Vec<&str> = listed.iter().map(|t| t.name.as_ref()).collect();
        assert_eq!(names, tool_names());
        for tool in &listed {
            let operation = branchyard::operations::by_tool(&tool.name).unwrap();
            let properties = &tool.input_schema["properties"];
            let listed: BTreeSet<String> =
                properties.as_object().unwrap().keys().cloned().collect();
            let expected: BTreeSet<String> = operation
                .params
                .iter()
                .filter_map(|p| p.tool)
                .map(|path| path.split('.').next().unwrap().to_owned())
                .collect();
            assert_eq!(listed, expected, "{}'s arguments", tool.name);
            // A nested argument, such as budget.max_usd, is in its object.
            for path in operation.params.iter().filter_map(|p| p.tool) {
                if let Some((outer, inner)) = path.split_once('.') {
                    let nested = &properties[outer]["properties"][inner];
                    assert!(nested.is_object(), "{} has no {path}", tool.name);
                }
            }
        }
    }
}
