// Derived from stablyai/orca src/main/github/client/update/
// resolve-review-thread.ts, at revision
// 280733273545f0b3eeedc1be54b14d406239030e.
// Copyright (c) 2026 Lovecast Inc. Licensed under the MIT License; the
// license text, which must accompany substantial portions of this code, is
// in vendor/orca/LICENSE.
// Modified for Branchyard: translated from TypeScript to Rust over
// Branchyard's `Gh` runner; Orca's repository resolution, connection
// options, rate-limit guard and semaphore are left out (`gh` chooses the
// repository and host, and the watch makes at most two calls per thread);
// a failure is returned as an error to record rather than logged, and
// `reply_to_review_thread` (Branchyard's own, not from Orca) posts the
// "Addressed in <commit>" reply before the thread is resolved.

//! Answering and resolving the review threads `by pr --watch` fed back,
//! through `gh api graphql`.

use serde_json::Value;

use crate::gh::{Gh, GhError};

/// Orca's mutation: resolve or unresolve one review thread by its node ID.
fn resolve_mutation(resolve: bool) -> String {
    let mutation = match resolve {
        true => "resolveReviewThread",
        false => "unresolveReviewThread",
    };
    format!(
        "mutation($threadId: ID!) {{ {mutation}(input: {{ threadId: $threadId }}) {{ thread {{ \
         isResolved }} }} }}"
    )
}

const REPLY_MUTATION: &str = "mutation($threadId: ID!, $body: String!) { \
     addPullRequestReviewThreadReply(input: { pullRequestReviewThreadId: $threadId, body: $body \
     }) { comment { id } } }";

/// `gh api graphql` on `host` (none for github.com) with `fields`.
fn graphql(gh: &Gh, host: Option<&str>, fields: &[String]) -> Result<Value, GhError> {
    let mut argv: Vec<&str> = vec!["api", "graphql"];
    if let Some(host) = host {
        argv.extend(["--hostname", host]);
    }
    for field in fields {
        argv.extend(["-f", field]);
    }
    gh.json(&argv, false)
}

/// Resolve (or unresolve) the thread `thread_id`; whether GitHub now
/// reports it in that state.
pub fn resolve_review_thread(
    gh: &Gh,
    host: Option<&str>,
    thread_id: &str,
    resolve: bool,
) -> Result<bool, GhError> {
    let mutation = match resolve {
        true => "resolveReviewThread",
        false => "unresolveReviewThread",
    };
    let value = graphql(
        gh,
        host,
        &[
            format!("query={}", resolve_mutation(resolve)),
            format!("threadId={thread_id}"),
        ],
    )?;
    Ok(value["data"][mutation]["thread"]["isResolved"].as_bool() == Some(resolve))
}

/// Post `body` as a reply in the thread `thread_id`.
pub fn reply_to_review_thread(
    gh: &Gh,
    host: Option<&str>,
    thread_id: &str,
    body: &str,
) -> Result<(), GhError> {
    graphql(
        gh,
        host,
        &[
            format!("query={REPLY_MUTATION}"),
            format!("threadId={thread_id}"),
            format!("body={body}"),
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mutation text is Orca's, so GitHub sees the same request.
    #[test]
    fn mutations_are_orcas() {
        assert_eq!(
            resolve_mutation(true),
            "mutation($threadId: ID!) { resolveReviewThread(input: { threadId: $threadId }) { \
             thread { isResolved } } }"
        );
        let orca = include_str!(
            "../../../vendor/orca/src/main/github/client/update/resolve-review-thread.ts"
        );
        assert!(orca.contains(
            "mutation($threadId: ID!) { ${mutation}(input: { threadId: $threadId }) { thread { \
             isResolved } } }"
        ));
        assert!(resolve_mutation(false).contains("unresolveReviewThread(input"));
    }
}
