//! Starting from an issue (`TaskRequest::issue`, `StartSessionRequest::issue`,
//! `GET /v1/repos/{repo}/issue`, `GET /v1/workspaces/{id}/issue`).
//!
//! The server does not fetch issues itself: it runs the operator-selected
//! `by` (`by_path`, else `by` on `PATH`) as `by issue --json REF`, in the
//! repository, with the server's own environment, so an issue resolves
//! exactly as `by run --issue` resolves it, with the same `gh` login, the
//! same Linear, Jira and GitLab credentials, `[trackers]` settings and
//! connector gateway, and one implementation of the prompt, the branch name
//! and the link (`docs/pull-requests.md`). Nothing from the request but the
//! reference and the extra instructions reaches that command, as arguments
//! after `--`, never through a shell. `BRANCHYARD_*` variables are removed
//! from its environment so a server started inside a branch cannot make it
//! act as that branch.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::ApiError;

/// How long fetching one issue may take.
const DEADLINE: Duration = Duration::from_secs(60);
/// The longest reference taken.
const MAX_REFERENCE: usize = 512;
/// How much of the command's complaint is quoted back.
const MAX_COMPLAINT: usize = 800;

/// An issue as `by issue --json` resolved it.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub(crate) struct Resolved {
    /// The link recorded on the branch: `number`, `url`, `title`, and for
    /// another tracker `tracker` and `key`.
    pub issue: Value,
    /// The branch name `by run --issue` would give it.
    pub branch: String,
    /// The prompt: the issue under a header, then any extra instructions.
    pub prompt: String,
}

impl Resolved {
    /// The issue's title, for a session's default name.
    pub fn title(&self) -> &str {
        self.issue["title"].as_str().unwrap_or_default()
    }
}

/// The `ref` of a query string (`?ref=%2312`), decoded.
pub(crate) fn reference_of(raw: Option<&str>) -> Option<String> {
    raw?.split('&')
        .filter_map(|part| part.split_once('='))
        .find(|(key, _)| *key == "ref")
        .map(|(_, value)| branchyard_client::http::decode_form(value))
        .filter(|value| !value.trim().is_empty())
}

fn unavailable(reference: &str, why: impl Into<String>, next: &str) -> ApiError {
    ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "issue_unavailable",
        format!("{} (next: {next})", why.into()),
    )
    .detail(json!({ "reference": reference, "next": next }))
}

/// Fetch `reference` by running `by issue --json` in `root`, with `extra`
/// added to the prompt.
pub(crate) async fn resolve(
    by: &Path,
    root: &Path,
    reference: &str,
    extra: &str,
) -> Result<Resolved, ApiError> {
    let reference = reference.trim();
    if reference.is_empty() || reference.len() > MAX_REFERENCE || reference.contains('\0') {
        return Err(ApiError::bad_request(
            "issue is empty, longer than 512 bytes, or not text",
        ));
    }
    let mut command = tokio::process::Command::new(by);
    command
        .args(["issue", "--json", "--"])
        .arg(reference)
        .arg(extra)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("BRANCHYARD_") {
            command.env_remove(name);
        }
    }
    command.env_remove("GIT_DIR").env_remove("GIT_WORK_TREE");
    let output = match tokio::time::timeout(DEADLINE, command.output()).await {
        Err(_) => {
            return Err(unavailable(
                reference,
                "fetching the issue took longer than 60 seconds",
                "retry; check that the issue's tracker is reachable from the server",
            ))
        }
        Ok(Err(_)) => {
            return Err(unavailable(
                reference,
                "the server could not run `by issue`",
                "ask the operator to set by_path to a by binary (docs/server.md)",
            ))
        }
        Ok(Ok(output)) => output,
    };
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        let said: String = said
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with("by: "))
            .collect::<Vec<_>>()
            .join(" / ");
        let said: String = said.chars().take(MAX_COMPLAINT).collect();
        return Err(unavailable(
            reference,
            if said.is_empty() {
                "the issue could not be fetched".to_owned()
            } else {
                format!("the issue could not be fetched: {said}")
            },
            "check the reference, and that the server's gh login or tracker credentials can read \
             it (docs/pull-requests.md)",
        ));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|e| ApiError::internal(format!("by issue printed something unexpected: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard_testkit::Scratch;
    use std::os::unix::fs::PermissionsExt;

    /// A fake `by` that answers `issue --json` from the script's body.
    fn fake_by(dir: &Scratch, body: &str) -> std::path::PathBuf {
        let path = dir.join("by");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn the_reference_is_the_decoded_ref_of_the_query() {
        assert_eq!(reference_of(Some("ref=%2312")).as_deref(), Some("#12"));
        assert_eq!(
            reference_of(Some("x=1&ref=linear%3AENG-1")).as_deref(),
            Some("linear:ENG-1")
        );
        assert_eq!(reference_of(Some("ref=")), None);
        assert_eq!(reference_of(None), None);
    }

    #[tokio::test]
    async fn an_issue_resolves_through_by_with_its_arguments_after_a_double_dash() {
        let dir = Scratch::new("issues");
        let by = fake_by(
            &dir,
            r#"printf '%s\n' "$*" > "$(dirname "$0")/args"
printf '{"issue": {"number": 12, "url": "u", "title": "T"}, "branch": "issue-12-t", "prompt": "P"}'"#,
        );
        let got = resolve(&by, dir.path(), "#12", "more").await.unwrap();
        assert_eq!(got.branch, "issue-12-t");
        assert_eq!(got.prompt, "P");
        assert_eq!(got.title(), "T");
        let args = std::fs::read_to_string(dir.join("args")).unwrap();
        assert_eq!(args.trim(), "issue --json -- #12 more");
    }

    #[tokio::test]
    async fn an_issue_that_cannot_be_fetched_is_refused_with_its_reason_and_a_next_step() {
        let dir = Scratch::new("issues");
        let by = fake_by(
            &dir,
            "echo 'by: fetching' >&2; echo 'error: Linear needs LINEAR_API_KEY' >&2; exit 3",
        );
        let error = resolve(&by, dir.path(), "linear:ENG-1", "")
            .await
            .unwrap_err();
        let text = format!("{error:?}");
        assert!(text.contains("issue_unavailable"), "{text}");
        assert!(text.contains("Linear needs LINEAR_API_KEY"), "{text}");
        assert!(!text.contains("by: fetching"), "{text}");
        assert!(text.contains("next:"), "{text}");
    }

    #[tokio::test]
    async fn nothing_that_is_not_a_reference_is_run() {
        let dir = Scratch::new("issues");
        let by = fake_by(&dir, "exit 0");
        for bad in ["", "   ", &"x".repeat(513)] {
            assert!(resolve(&by, dir.path(), bad, "").await.is_err());
        }
        let missing = dir.join("no-such-by");
        let error = resolve(&missing, dir.path(), "#1", "").await.unwrap_err();
        assert!(format!("{error:?}").contains("by_path"));
    }
}
