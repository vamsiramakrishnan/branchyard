//! An external, authenticated workspace manager over MCP on stdio.
//!
//! These tools call the ordinary HTTP server with an operator-provided
//! credential. They are never added to a branch's descendant-scoped server.

mod cli;
mod output;
mod tools;

use std::borrow::Cow;

use branchyard_client::manager_api::{Lifecycle, PreviewOptions, Read, Write};
use branchyard_client::Client;
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde_json::{json, Map, Value};

pub use cli::{main_with_args, serve_configured};

const INSTRUCTIONS: &str = "Manage authorized Branchyard projects, workspaces and native agent conversations. \
Knowledge work can start without a user Git repository and can finish with immutable deliverables, without a Git diff. \
Use one stable idempotency_key per intended mutation and reuse it on retries. \
For workspace_action, pass revision from inspect_workspace. On revision_conflict inspect again and explicitly propose a new action with a new key; never automatically refresh and resubmit. \
Accepted operations continue after an MCP disconnect; inspect or wait on their operation IDs. Starting a conversation does not replace any native lead, plan or \
subagents. Review current outputs and contracts before finalizing. Workspace permissions do not grant worker IAM or \
upstream connector permissions. Tools never accept arbitrary shell commands, filesystem destinations or request URLs. \
This external manager uses its own HTTP credential and is distinct from the restricted in-task delegation MCP. \
Tool responses have a bounded context budget; oversized responses return a structured error with small identifiers and cursors. \
Preview defaults to 25 rows. Use narrower previews, authorized original downloads or focused compute for large tables. \
A mutation can succeed before its response is found too large; inspect its operation and reuse the original idempotency_key. \
For a harness that cannot route tool approvals, start_session requires explicit unapproved_tools:true and the operator's permission. It defaults to false; never infer consent.";

/// Tool definitions for the external manager only.
pub fn tools() -> Vec<Tool> {
    tools::tools()
}

/// External workspace control under one HTTP principal's authority.
#[derive(Clone, Debug)]
pub struct Server {
    client: Client,
}

impl Server {
    /// Use the existing client's token, TLS configuration and request limits.
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    /// Validate a manager tool and send its fixed-route HTTP request.
    pub fn call(&self, name: &str, arguments: Value) -> Result<Value, CallError> {
        tools::validate(name, &arguments).map_err(CallError::Arguments)?;
        let mut args = arguments
            .as_object()
            .cloned()
            .ok_or_else(|| CallError::Arguments("arguments must be an object".into()))?;
        let workspace = optional_text(&args, "workspace_id").unwrap_or_default();
        let session = optional_text(&args, "session_id").unwrap_or_default();
        let artifact = optional_text(&args, "artifact_id").unwrap_or_default();
        let read = match name {
            "list_harnesses" => Some(Read::Harnesses),
            "worker_inventory" => Some(Read::Inventory),
            "list_projects" => Some(Read::Projects),
            "list_workspaces" => Some(Read::Workspaces(optional_text(&args, "project_id"))),
            "inspect_workspace" => Some(Read::Workspace(workspace.clone())),
            "list_sessions" => Some(Read::Sessions(workspace.clone())),
            "session_messages" => Some(Read::Messages {
                workspace: workspace.clone(),
                session: session.clone(),
                after: optional_text(&args, "after"),
            }),
            "list_sources" => Some(Read::Sources(workspace.clone())),
            "list_artifacts" => Some(Read::Artifacts(workspace.clone())),
            "inspect_artifact" => Some(Read::Artifact {
                workspace: workspace.clone(),
                artifact: artifact.clone(),
            }),
            "preview_artifact" => Some(Read::Preview {
                workspace: workspace.clone(),
                artifact: artifact.clone(),
                options: PreviewOptions {
                    view: optional_text(&args, "view").unwrap_or_else(|| "auto".into()),
                    page: u32::try_from(number(&args, "page", 1)).map_err(|_| {
                        CallError::Arguments("page exceeds its supported range".into())
                    })?,
                    sheet: optional_text(&args, "sheet"),
                    offset: u32::try_from(number(&args, "offset", 0)).map_err(|_| {
                        CallError::Arguments("offset exceeds its supported range".into())
                    })?,
                    limit: u16::try_from(number(&args, "limit", 25)).map_err(|_| {
                        CallError::Arguments("limit exceeds its supported range".into())
                    })?,
                },
            }),
            "operation_status" | "wait_operation" => Some(Read::Operation {
                id: text(&args, "operation_id")?,
                wait_seconds: if name == "wait_operation" {
                    u8::try_from(number(&args, "seconds", 30)).map_err(|_| {
                        CallError::Arguments("wait exceeds its supported range".into())
                    })?
                } else {
                    0
                },
            }),
            _ => None,
        };
        if let Some(read) = read {
            return self.client.manager_read(&read).map_err(CallError::Http);
        }
        let key = text(&args, "idempotency_key")?;
        args.remove("idempotency_key");
        args.remove("workspace_id");
        args.remove("session_id");
        let write = match name {
            "create_project" => Write::CreateProject(Value::Object(args)),
            "create_workspace" => Write::CreateWorkspace(Value::Object(args)),
            "start_session" => Write::StartSession {
                workspace,
                request: Value::Object(args),
            },
            "send_message" | "steer_session" | "resume_session" => {
                let action = match name {
                    "steer_session" => "steer",
                    "resume_session" => "resume",
                    _ => "send",
                };
                args.insert("action".into(), json!(action));
                Write::Message {
                    workspace,
                    session,
                    request: Value::Object(args),
                }
            }
            "cancel_session" => Write::Cancel { workspace, session },
            "finalize_deliverables" => Write::Finalize {
                workspace,
                request: Value::Object(args),
            },
            "workspace_action" => {
                let action = match text(&args, "action")?.as_str() {
                    "sleep" => Lifecycle::Sleep,
                    "wake" => Lifecycle::Wake,
                    "archive" => Lifecycle::Archive,
                    "restore" => Lifecycle::Restore,
                    _ => return Err(CallError::Arguments("unsupported workspace action".into())),
                };
                let revision = args
                    .get("revision")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| {
                        CallError::Arguments("revision must be an unsigned integer".into())
                    })?;
                Write::Lifecycle {
                    workspace,
                    revision,
                    action,
                }
            }
            _ => {
                return Err(CallError::Arguments(format!(
                    "no manager tool named {name}"
                )))
            }
        };
        self.client
            .manager_write(&write, &key)
            .map_err(CallError::Http)
    }
}

fn optional_text(args: &Map<String, Value>, name: &str) -> Option<String> {
    args.get(name).and_then(Value::as_str).map(str::to_owned)
}

fn text(args: &Map<String, Value>, name: &str) -> Result<String, CallError> {
    optional_text(args, name).ok_or_else(|| CallError::Arguments(format!("{name} is required")))
}

fn number(args: &Map<String, Value>, name: &str, default: u64) -> u64 {
    args.get(name).and_then(Value::as_u64).unwrap_or(default)
}

/// A rejected argument or a normal server/client error.
#[derive(Debug)]
pub enum CallError {
    /// The supplied tool arguments do not match its closed schema.
    Arguments(String),
    /// The authenticated HTTP request failed.
    Http(branchyard_client::Error),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Arguments(message) => f.write_str(message),
            Self::Http(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for CallError {}

impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "branchyard-workspace-manager",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(tools())
            .with_ttl_ms(0)
            .with_cache_scope(CacheScope::Private))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let name = request.name.to_string();
        let arguments = request.arguments.map_or_else(|| json!({}), Value::Object);
        tools::validate(&name, &arguments)
            .map_err(|e| ErrorData::invalid_params(Cow::Owned(e), None))?;
        let server = self.clone();
        let result = tokio::task::spawn_blocking(move || server.call(&name, arguments))
            .await
            .map_err(|e| ErrorData::internal_error(Cow::Owned(e.to_string()), None))?;
        let (value, failed) = match result {
            Ok(value) => (value, false),
            Err(error) => {
                let detail = match &error {
                    CallError::Arguments(_) => {
                        json!({"code":"invalid_arguments","message":error.to_string()})
                    }
                    CallError::Http(branchyard_client::Error::Api { status, error }) => {
                        json!({"status":status,"error":error})
                    }
                    CallError::Http(_) => {
                        json!({"code":"request_failed","message":error.to_string()})
                    }
                };
                (detail, true)
            }
        };
        Ok(output::bounded_result(value, failed)?.into())
    }
}

/// Serve the authenticated external manager until its stdio client disconnects.
pub fn serve_stdio(client: Client) -> std::io::Result<()> {
    ensure_operator()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    runtime.block_on(async move {
        let running = Server::new(client)
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

fn ensure_operator() -> std::io::Result<()> {
    if [branchyard::ENV_BRANCH, branchyard::ENV_TOKEN]
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "the external workspace manager cannot run inside a managed harness turn; use its branch-scoped delegation tools",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {

    #[test]
    fn lifecycle_revision_is_required_and_conflicts_are_not_retried() {
        let http = branchyard_testkit::MockHttp::start(|request| {
            assert_eq!(request.path, "/v1/workspaces/w/actions/archive");
            assert_eq!(request.json(), json!({"revision":7}));
            branchyard_testkit::Response::json(
                409,
                &json!({"error":{"code":"revision_conflict","message":"inspect the workspace before proposing a new action"}}),
            )
        });
        let server = Server::new(Client::new(&http.url(), "fixture").unwrap());
        let error = server.call("workspace_action", json!({"workspace_id":"w","action":"archive","revision":7,"idempotency_key":"archive-once"})).unwrap_err();
        assert!(
            matches!(
                error,
                CallError::Http(branchyard_client::Error::Api { status: 409, .. })
            ),
            "{error:?}"
        );
        assert_eq!(http.requests().len(), 1);
        for revision in [None, Some(json!(-1)), Some(json!(1.5)), Some(json!("7"))] {
            let mut args =
                json!({"workspace_id":"w","action":"archive","idempotency_key":"archive-once"});
            if let Some(value) = revision {
                args["revision"] = value;
            }
            assert!(matches!(
                server.call("workspace_action", args),
                Err(CallError::Arguments(_))
            ));
        }
        assert_eq!(http.requests().len(), 1);
    }
    use super::*;
    use branchyard_testkit::{MockHttp, Response};

    #[test]
    fn knowledge_project_and_native_session_forward_without_extra_authority() {
        let http = MockHttp::start(|_| Response::json(202, &json!({"id":"op"})));
        let server = Server::new(Client::new(&http.url(), "fixture-secret").unwrap());
        server.call("create_project",json!({"name":"Research","source":{"kind":"knowledge"},"result_mode":"artifacts","idempotency_key":"project-key"})).unwrap();
        server.call("start_session",json!({"workspace_id":"w","name":"Researcher","prompt":"Check supplied sources","harness":"antigravity","access":"writer","unapproved_tools":true,"idempotency_key":"session-key"})).unwrap();
        server.call("steer_session",json!({"workspace_id":"w","session_id":"s","prompt":"Preserve source quotations","idempotency_key":"steer-key"})).unwrap();
        let requests = http.requests();
        assert_eq!(requests[0].json()["source"], json!({"kind":"knowledge"}));
        assert_eq!(requests[1].path, "/v1/workspaces/w/sessions");
        assert!(requests[1].json().get("workspace_id").is_none());
        assert_eq!(requests[1].json()["unapproved_tools"], true);
        assert_eq!(requests[2].json()["action"], "steer");
        assert_eq!(requests[2].header("idempotency-key"), Some("steer-key"));
        assert!(!format!("{server:?}").contains("fixture-secret"));
    }

    #[test]
    fn manager_rejects_shell_paths_unknown_arguments_and_preserves_api_denials() {
        let http = MockHttp::start(|_| {
            Response::json(
                403,
                &json!({"error":{"code":"denied","message":"membership revoked"}}),
            )
        });
        let server = Server::new(Client::new(&http.url(), "fixture").unwrap());
        assert!(matches!(
            server.call(
                "inspect_workspace",
                json!({"workspace_id":"w","url":"http://elsewhere"})
            ),
            Err(CallError::Arguments(_))
        ));
        assert!(matches!(
            server.call("spawn", json!({"command":"sh"})),
            Err(CallError::Arguments(_))
        ));
        let error = server.call("list_projects", json!({})).unwrap_err();
        assert!(matches!(
            error,
            CallError::Http(branchyard_client::Error::Api { status: 403, .. })
        ));
        assert_eq!(http.requests().len(), 1);
    }

    #[test]
    fn every_tool_has_a_valid_dispatch_shape() {
        let http = MockHttp::start(|_| Response::json(200, &json!({"ok":true})));
        let server = Server::new(Client::new(&http.url(), "fixture").unwrap());
        for (name, args) in [
            ("list_harnesses", json!({})),
            ("worker_inventory", json!({})),
            ("list_projects", json!({})),
            ("list_workspaces", json!({"project_id":"p"})),
            (
                "create_workspace",
                json!({"project_id":"p","title":"Brief","idempotency_key":"k"}),
            ),
            ("inspect_workspace", json!({"workspace_id":"w"})),
            ("list_sessions", json!({"workspace_id":"w"})),
            (
                "session_messages",
                json!({"workspace_id":"w","session_id":"s","after":"1"}),
            ),
            (
                "send_message",
                json!({"workspace_id":"w","session_id":"s","prompt":"go","idempotency_key":"k"}),
            ),
            (
                "resume_session",
                json!({"workspace_id":"w","session_id":"s","prompt":"continue","idempotency_key":"k"}),
            ),
            (
                "cancel_session",
                json!({"workspace_id":"w","session_id":"s","idempotency_key":"k"}),
            ),
            ("list_sources", json!({"workspace_id":"w"})),
            ("list_artifacts", json!({"workspace_id":"w"})),
            (
                "inspect_artifact",
                json!({"workspace_id":"w","artifact_id":"a"}),
            ),
            (
                "preview_artifact",
                json!({"workspace_id":"w","artifact_id":"a","view":"table","limit":1,"sheet":"Q1"}),
            ),
            (
                "finalize_deliverables",
                json!({"workspace_id":"w","revision":1,"artifacts":["a"],"idempotency_key":"k"}),
            ),
            (
                "workspace_action",
                json!({"workspace_id":"w","action":"sleep","revision":1,"idempotency_key":"k"}),
            ),
            ("operation_status", json!({"operation_id":"o"})),
            ("wait_operation", json!({"operation_id":"o","seconds":0})),
        ] {
            server.call(name, args).unwrap();
        }
        server
            .call(
                "preview_artifact",
                json!({"workspace_id":"w","artifact_id":"a"}),
            )
            .unwrap();
        let requests = http.requests();
        assert_eq!(requests.len(), 20);
        assert!(requests.last().unwrap().path.contains("limit=25"));
    }
}
