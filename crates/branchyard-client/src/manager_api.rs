//! Restricted routes used by an external workspace manager.
//!
//! This facade has no arbitrary request URL or shell operation. Every call
//! reaches the ordinary server authorization and durable admission paths.

use serde_json::Value;

use crate::http::encode;
use crate::{Client, Error};

/// A collection or resource that a workspace manager may read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Read {
    /// Configured native harness profiles and their reported readiness.
    Harnesses,
    /// Workers and their advertised execution capabilities.
    Inventory,
    /// Projects visible to the current principal.
    Projects,
    /// Workspaces, optionally limited to one project.
    Workspaces(Option<String>),
    /// One workspace and its authorized execution references.
    Workspace(String),
    /// Conversations attached to a workspace.
    Sessions(String),
    /// Messages after an opaque server cursor.
    Messages {
        /// Owning workspace.
        workspace: String,
        /// Conversation identifier.
        session: String,
        /// Optional continuation cursor.
        after: Option<String>,
    },
    /// Immutable source metadata.
    Sources(String),
    /// Immutable output metadata and provenance.
    Artifacts(String),
    /// One artifact's metadata, never a local file download.
    Artifact {
        /// Owning workspace.
        workspace: String,
        /// Immutable artifact identifier.
        artifact: String,
    },
    /// A bounded artifact preview.
    Preview {
        /// Owning workspace.
        workspace: String,
        /// Immutable artifact identifier.
        artifact: String,
        /// Validated presentation options.
        options: PreviewOptions,
    },
    /// Current operation state, optionally long-polling for at most 30 seconds.
    Operation {
        /// Server operation identifier.
        id: String,
        /// Long-poll duration, bounded to 30 seconds.
        wait_seconds: u8,
    },
}

/// Bounded document/table options; source and output paths are server-owned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewOptions {
    /// `auto`, `table`, or `pages`.
    pub view: String,
    /// One-based page or slide number.
    pub page: u32,
    /// Optional workbook sheet name.
    pub sheet: Option<String>,
    /// First table row to return.
    pub offset: u32,
    /// At most 1,000 table rows.
    pub limit: u16,
}

impl Default for PreviewOptions {
    fn default() -> Self {
        Self {
            view: "auto".into(),
            page: 1,
            sheet: None,
            offset: 0,
            limit: 100,
        }
    }
}

/// One admitted manager mutation. Bodies use the server's normal wire schema.
#[derive(Clone, Debug, PartialEq)]
pub enum Write {
    /// Register a repository-backed or knowledge project.
    CreateProject(Value),
    /// Reserve a task-backed workspace.
    CreateWorkspace(Value),
    /// Start an isolated native conversation.
    StartSession {
        /// Owning workspace.
        workspace: String,
        /// The normal session creation request.
        request: Value,
    },
    /// Send, steer, or resume an existing native conversation.
    Message {
        /// Owning workspace.
        workspace: String,
        /// Native conversation binding.
        session: String,
        /// The normal message request, including its action.
        request: Value,
    },
    /// Cancel the current conversation operation.
    Cancel {
        /// Owning workspace.
        workspace: String,
        /// Native conversation binding.
        session: String,
    },
    /// Accept immutable deliverables at a checked revision.
    Finalize {
        /// Owning workspace.
        workspace: String,
        /// The normal finalization request.
        request: Value,
    },
    /// A supported lifecycle action.
    Lifecycle {
        /// Owning workspace.
        workspace: String,
        /// The desired action.
        action: Lifecycle,
    },
}

/// Lifecycle choices exposed to a manager; capability checks remain on the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lifecycle {
    /// Suspend supported compute while retaining work.
    Sleep,
    /// Resume supported compute.
    Wake,
    /// Archive while retaining original sources and accepted deliverables.
    Archive,
    /// Restore an archived workspace.
    Restore,
}

impl Lifecycle {
    /// The fixed URL segment for this action.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sleep => "sleep",
            Self::Wake => "wake",
            Self::Archive => "archive",
            Self::Restore => "restore",
        }
    }
}

fn workspace_path(id: &str) -> String {
    format!("/v1/workspaces/{}", encode(id))
}

fn session_path(workspace: &str, session: &str) -> String {
    format!("{}/sessions/{}", workspace_path(workspace), encode(session))
}

fn read_path(read: &Read) -> Result<String, Error> {
    Ok(match read {
        Read::Harnesses => "/v1/harnesses".into(),
        Read::Inventory => "/v1/inventory".into(),
        Read::Projects => "/v1/projects".into(),
        Read::Workspaces(project) => match project {
            Some(project) => format!("/v1/workspaces?project={}", encode(project)),
            None => "/v1/workspaces".into(),
        },
        Read::Workspace(id) => workspace_path(id),
        Read::Sessions(id) => format!("{}/sessions", workspace_path(id)),
        Read::Messages {
            workspace,
            session,
            after,
        } => {
            let path = format!("{}/messages", session_path(workspace, session));
            match after {
                Some(after) => format!("{path}?after={}", encode(after)),
                None => path,
            }
        }
        Read::Sources(id) => format!("{}/sources", workspace_path(id)),
        Read::Artifacts(id) => format!("{}/artifacts", workspace_path(id)),
        Read::Artifact {
            workspace,
            artifact,
        } => {
            format!(
                "{}/artifacts/{}",
                workspace_path(workspace),
                encode(artifact)
            )
        }
        Read::Preview {
            workspace,
            artifact,
            options,
        } => {
            if !["auto", "table", "pages"].contains(&options.view.as_str())
                || options.page == 0
                || !(1..=1000).contains(&options.limit)
            {
                return Err(Error::Config(
                    "invalid preview view, page or row limit".into(),
                ));
            }
            let mut path = format!(
                "{}/artifacts/{}/preview?view={}&page={}&offset={}&limit={}",
                workspace_path(workspace),
                encode(artifact),
                encode(&options.view),
                options.page,
                options.offset,
                options.limit
            );
            if let Some(sheet) = &options.sheet {
                path.push_str(&format!("&sheet={}", encode(sheet)));
            }
            path
        }
        Read::Operation { id, wait_seconds } => {
            if *wait_seconds > 30 {
                return Err(Error::Config(
                    "operation wait must be at most 30 seconds".into(),
                ));
            }
            format!("/v1/operations/{}?wait={wait_seconds}", encode(id))
        }
    })
}

fn write_parts(write: &Write) -> (String, Value) {
    match write {
        Write::CreateProject(value) => ("/v1/projects".into(), value.clone()),
        Write::CreateWorkspace(value) => ("/v1/workspaces".into(), value.clone()),
        Write::StartSession { workspace, request } => (
            format!("{}/sessions", workspace_path(workspace)),
            request.clone(),
        ),
        Write::Message {
            workspace,
            session,
            request,
        } => (
            format!("{}/messages", session_path(workspace, session)),
            request.clone(),
        ),
        Write::Cancel { workspace, session } => (
            format!("{}/cancel", session_path(workspace, session)),
            serde_json::json!({}),
        ),
        Write::Finalize { workspace, request } => (
            format!("{}/finalize", workspace_path(workspace)),
            request.clone(),
        ),
        Write::Lifecycle { workspace, action } => (
            format!("{}/actions/{}", workspace_path(workspace), action.as_str()),
            serde_json::json!({}),
        ),
    }
}

impl Client {
    /// Read an explicitly supported workspace-manager resource.
    pub fn manager_read(&self, read: &Read) -> Result<Value, Error> {
        let path = read_path(read)?;
        let extra = match read {
            Read::Operation { wait_seconds, .. } => {
                std::time::Duration::from_secs(u64::from(*wait_seconds))
            }
            _ => std::time::Duration::ZERO,
        };
        let response = self.call("GET", &path, None, &[], self.timeout + extra)?;
        self.decode(response)
    }

    /// Submit a manager mutation using the caller's stable idempotency key.
    pub fn manager_write(&self, write: &Write, key: &str) -> Result<Value, Error> {
        if key.is_empty() || key.len() > 256 || key.chars().any(char::is_control) {
            return Err(Error::Config(
                "idempotency key must contain 1 to 256 bytes without controls".into(),
            ));
        }
        let (path, body) = write_parts(write);
        self.post(&path, &body, key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard_testkit::{MockHttp, Response};
    use serde_json::json;

    #[test]
    fn routes_are_closed_and_encode_identifiers_and_queries() {
        let server = MockHttp::start(|_| Response::json(200, &json!({"ok":true})));
        let client = Client::new(&server.url(), "fixture-token").unwrap();
        let reads = [
            Read::Projects,
            Read::Workspaces(Some("p/a&b".into())),
            Read::Workspace("w/a".into()),
            Read::Sessions("w/a".into()),
            Read::Messages {
                workspace: "w".into(),
                session: "s/a".into(),
                after: Some("a&b".into()),
            },
            Read::Sources("w".into()),
            Read::Artifacts("w".into()),
            Read::Artifact {
                workspace: "w".into(),
                artifact: "a/b".into(),
            },
            Read::Preview {
                workspace: "w".into(),
                artifact: "a".into(),
                options: PreviewOptions {
                    sheet: Some("Q & A".into()),
                    ..PreviewOptions::default()
                },
            },
            Read::Operation {
                id: "op/a".into(),
                wait_seconds: 30,
            },
            Read::Harnesses,
            Read::Inventory,
        ];
        for read in reads {
            assert_eq!(client.manager_read(&read).unwrap(), json!({"ok":true}));
        }
        let requests = server.requests();
        assert_eq!(requests.len(), 12);
        assert_eq!(requests[10].path, "/v1/harnesses");
        assert_eq!(requests[11].path, "/v1/inventory");
        assert_eq!(requests[1].path, "/v1/workspaces?project=p%2Fa%26b");
        assert_eq!(requests[2].path, "/v1/workspaces/w%2Fa");
        assert!(requests[4].path.ends_with("s%2Fa/messages?after=a%26b"));
        assert!(requests[8].path.ends_with("&sheet=Q%20%26%20A"));
        assert!(requests
            .iter()
            .all(|r| r.header("authorization") == Some("Bearer fixture-token")));
    }

    #[test]
    fn all_mutations_reuse_the_supplied_idempotency_key() {
        let server = MockHttp::start(|_| Response::json(202, &json!({"id":"op"})));
        let client = Client::new(&server.url(), "fixture-token").unwrap();
        let writes = [
            Write::CreateProject(json!({"name":"Research"})),
            Write::CreateWorkspace(json!({"title":"Brief"})),
            Write::StartSession {
                workspace: "w".into(),
                request: json!({"prompt":"research"}),
            },
            Write::Message {
                workspace: "w".into(),
                session: "s".into(),
                request: json!({"prompt":"continue"}),
            },
            Write::Cancel {
                workspace: "w".into(),
                session: "s".into(),
            },
            Write::Finalize {
                workspace: "w".into(),
                request: json!({"revision":2,"artifacts":["a"]}),
            },
            Write::Lifecycle {
                workspace: "w".into(),
                action: Lifecycle::Archive,
            },
        ];
        for write in writes {
            client.manager_write(&write, "same-key").unwrap();
        }
        let requests = server.requests();
        assert_eq!(requests.len(), 7);
        assert!(requests
            .iter()
            .all(|r| r.method == "POST" && r.header("idempotency-key") == Some("same-key")));
        assert_eq!(requests[6].path, "/v1/workspaces/w/actions/archive");
        assert_eq!(requests[5].json()["revision"], 2);
        assert_eq!(Lifecycle::Sleep.as_str(), "sleep");
        assert_eq!(Lifecycle::Wake.as_str(), "wake");
        assert_eq!(Lifecycle::Restore.as_str(), "restore");
    }

    #[test]
    fn invalid_bounds_fail_before_a_request_and_api_errors_keep_their_code() {
        let client = Client::new("http://127.0.0.1:1", "fixture").unwrap();
        assert!(client
            .manager_read(&Read::Operation {
                id: "x".into(),
                wait_seconds: 31
            })
            .is_err());
        assert!(client
            .manager_read(&Read::Preview {
                workspace: "w".into(),
                artifact: "a".into(),
                options: PreviewOptions {
                    page: 0,
                    ..PreviewOptions::default()
                }
            })
            .is_err());
        assert!(client
            .manager_write(&Write::CreateProject(json!({})), "\n")
            .is_err());
        let server = MockHttp::start(|_| {
            Response::json(
                403,
                &json!({"error":{"code":"denied","message":"workspace access revoked"}}),
            )
        });
        let error = Client::new(&server.url(), "fixture")
            .unwrap()
            .manager_read(&Read::Workspace("w".into()))
            .unwrap_err();
        assert_eq!(error.code(), Some("denied"));
        assert!(!format!("{client:?}").contains("fixture"));
    }
}
