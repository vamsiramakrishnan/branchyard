//! A branch's diff file by file and review comments on it over real HTTP on
//! 127.0.0.1, with the fake ACP agent as the `gemini-cli` harness: the
//! structured changes, a review delivered as the next turn and as a steer
//! (one prompt that names the file, the lines and the code they quote, and
//! one feedback record), a stale or unplaceable review refused, a replayed
//! key sending once, and a later candidate marking the comments it
//! addressed.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use branchyard::{Activity, DiffSide, FeedbackSource, FileStatus, ReviewComment};
use branchyard_client::api::{
    OperationState, ReviewDelivery, ReviewRequest, SendRequest, TaskRequest,
};
use branchyard_client::new_key;
use branchyard_testkit::wait;
use common::{await_operation, run, Fixture, Server};

/// A task for the fake agent in a private home: a person's approval policy
/// binds it only there, and a later send inherits it.
fn task(prompt: &str, name: &str) -> TaskRequest {
    TaskRequest {
        isolated: true,
        ..common::task(prompt, name)
    }
}

/// A prompt for the fake agent that writes `notes.txt` with these lines.
fn notes(lines: &[&str]) -> String {
    format!("SH printf '{}\\n' > notes.txt", lines.join("\\n"))
}

fn comment(start: Option<u32>, line: u32, body: &str) -> ReviewComment {
    ReviewComment {
        path: "notes.txt".into(),
        line,
        start_line: start,
        side: DiffSide::New,
        body: body.into(),
    }
}

fn prompts(events: &[branchyard::RecordedEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Prompt(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_review_is_one_prompt_with_the_lines_it_is_about_and_marks_what_a_later_turn_addressed() {
    let f = Fixture::new();
    let server = Server::start(f.config());
    let client = server.client();
    let repo = client.repo("app");

    // No candidate yet: nothing to show, nothing to review.
    let empty = run(&client, &task("just say hi", "quiet"));
    assert_eq!(empty.state, OperationState::Succeeded, "{empty:?}");
    let none = repo.changes("quiet").unwrap();
    assert!(none.files.is_empty() && none.commit.is_none(), "{none:?}");
    assert_eq!(
        repo.changes("missing").unwrap_err().code(),
        Some("unknown_branch")
    );

    let made = run(
        &client,
        &task(&notes(&["alpha", "beta", "gamma", "delta"]), "rev"),
    );
    assert_eq!(made.state, OperationState::Succeeded, "{made:?}");
    let changes = repo.changes("rev").unwrap();
    assert_eq!(changes.files.len(), 1, "{changes:?}");
    let file = &changes.files[0];
    assert_eq!(file.path, "notes.txt");
    assert_eq!(file.status, FileStatus::Added);
    assert_eq!((file.additions, file.deletions), (4, 0));
    assert_eq!((changes.additions, changes.deletions), (4, 0));
    assert_eq!(file.hunks[0].lines[1].text, "beta");
    assert_eq!(file.hunks[0].lines[1].new, Some(2));
    let seen = changes.commit.clone().expect("a candidate");
    assert_eq!(
        repo.changes("rev").unwrap(),
        changes,
        "reading is stable and writes nothing"
    );

    // Refused: a candidate the reviewer did not read, lines the diff does
    // not show, and nothing to say.
    let send = |request: &ReviewRequest| repo.review("rev", request, &new_key());
    let stale = send(&ReviewRequest {
        comments: vec![comment(None, 1, "x")],
        commit: Some("0123456789abcdef".into()),
        ..ReviewRequest::default()
    })
    .unwrap_err();
    assert_eq!(stale.code(), Some("stale_candidate"), "{stale}");
    let unplaced = send(&ReviewRequest {
        comments: vec![comment(None, 40, "x")],
        ..ReviewRequest::default()
    })
    .unwrap_err();
    assert_eq!(unplaced.code(), Some("usage"), "{unplaced}");
    assert!(unplaced.to_string().contains("not all in the diff"));
    let nothing = send(&ReviewRequest::default()).unwrap_err();
    assert_eq!(nothing.code(), Some("usage"), "{nothing}");
    assert!(repo.reviews("rev").unwrap().reviews.is_empty());

    // A review as the next turn: one prompt, one feedback record.
    let key = new_key();
    let request = ReviewRequest {
        comments: vec![
            comment(Some(1), 2, "Name these better.\nBoth of them."),
            comment(None, 4, "Drop this line."),
        ],
        note: Some("Mostly fine".into()),
        commit: Some(seen.clone()),
        delivery: ReviewDelivery::NextTurn,
    };
    let sent = repo.review("rev", &request, &key).unwrap();
    assert_eq!(
        (sent.delivery, sent.comments),
        (ReviewDelivery::NextTurn, 2)
    );
    assert!(sent.prompt.contains(
        "File: notes.txt\nLines: 1-2\nQuoted lines:\n> alpha\n> beta\n\
         User comment: \"Name these better.\\nBoth of them.\""
    ));
    assert!(sent
        .prompt
        .contains("File: notes.txt\nLine: 4\nQuoted lines:\n> delta\n"));
    assert!(sent.prompt.contains("Overall note: \"Mostly fine\""));
    let op = await_operation(&client, &sent.operation.clone().expect("an operation"));
    assert_eq!(op.state, OperationState::Succeeded, "{op:?}");
    // The same key sends nothing more and names the same operation.
    let again = repo.review("rev", &request, &key).unwrap();
    assert_eq!(again.operation, sent.operation);

    let events = repo.events("rev", 0).unwrap().events;
    let review_prompts: Vec<&str> = prompts(&events)
        .into_iter()
        .filter(|p| p.contains("Review comments on this branch's changes"))
        .collect();
    assert_eq!(review_prompts.len(), 1, "{review_prompts:?}");
    assert!(review_prompts[0].contains("> alpha\n> beta\n"));
    let feedback: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Feedback(f) => Some(f),
            _ => None,
        })
        .collect();
    assert_eq!(feedback.len(), 1);
    assert_eq!(feedback[0].source, FeedbackSource::Review);
    assert_eq!(feedback[0].reference.as_deref(), Some(seen.as_str()));
    assert_eq!(feedback[0].items.len(), 3, "two comments and the note");
    assert_eq!(feedback[0].items[0].start_line, Some(1));
    assert_eq!(feedback[0].items[0].side, Some(DiffSide::New));

    // Stored with the branch: read back, nothing addressed yet.
    let reviews = repo.reviews("rev").unwrap();
    assert_eq!(reviews.reviews.len(), 1, "{reviews:?}");
    let review = &reviews.reviews[0];
    assert_eq!(review.note.as_deref(), Some("Mostly fine"));
    assert_eq!(review.commit.as_deref(), Some(seen.as_str()));
    assert_eq!(review.comments.len(), 2);
    assert_eq!(review.comments[0].start_line, Some(1));
    assert!(review.comments.iter().all(|c| c.addressed_in.is_none()));

    // The next candidate rewrites line 2 only: the comment on lines 1-2 is
    // addressed in it, the one on line 4 stands.
    let fixed = repo
        .send(
            "rev",
            &SendRequest {
                prompt: notes(&["alpha", "BETA", "gamma", "delta"]),
                policy: branchyard_client::api::PolicySpec::allow_all(),
                ..SendRequest::default()
            },
            &new_key(),
        )
        .unwrap();
    await_operation(&client, &fixed.id);
    let now = repo.changes("rev").unwrap().commit.unwrap();
    assert_ne!(now, seen);
    let reviews = repo.reviews("rev").unwrap();
    assert_eq!(reviews.candidate.as_deref(), Some(now.as_str()));
    let comments = &reviews.reviews[0].comments;
    assert_eq!(comments[0].addressed_in.as_deref(), Some(now.as_str()));
    assert_eq!(comments[1].addressed_in, None);
    // The old candidate's numbers no longer place a comment.
    let late = send(&ReviewRequest {
        comments: vec![comment(None, 2, "x")],
        commit: Some(seen),
        ..ReviewRequest::default()
    })
    .unwrap_err();
    assert_eq!(late.code(), Some("stale_candidate"), "{late}");
}

#[test]
fn a_review_can_steer_the_running_turn_and_is_refused_when_none_runs() {
    let f = Fixture::new();
    let server = Server::start(f.config());
    let client = server.client();
    let repo = client.repo("app");
    let made = run(&client, &task(&notes(&["one", "two", "three"]), "steady"));
    assert_eq!(made.state, OperationState::Succeeded, "{made:?}");
    let commit = repo.changes("steady").unwrap().commit;
    let request = ReviewRequest {
        comments: vec![comment(None, 3, "Why three?")],
        commit,
        delivery: ReviewDelivery::Steer,
        ..ReviewRequest::default()
    };
    // Idle: there is no turn to steer, and nothing is recorded.
    let idle = repo.review("steady", &request, &new_key()).unwrap_err();
    assert_eq!(idle.code(), Some("not_running"), "{idle}");
    assert!(repo.reviews("steady").unwrap().reviews.is_empty());

    let waiting = repo
        .send(
            "steady",
            &SendRequest {
                prompt: "AWAIT_STEER".into(),
                ..SendRequest::default()
            },
            &new_key(),
        )
        .unwrap();
    let steered = wait::until("the turn to take a steer", || {
        repo.review("steady", &request, &new_key())
            .map_err(|e| e.to_string())
    });
    assert_eq!(steered.delivery, ReviewDelivery::Steer);
    assert!(steered.operation.is_none() && steered.steer.is_some());

    let op = await_operation(&client, &waiting.id);
    assert_eq!(op.state, OperationState::Succeeded, "{op:?}");
    let steer = steered.steer.expect("the steer");
    assert!(steer
        .text
        .contains("Review comments on this branch's changes"));
    assert!(steer
        .text
        .contains("File: notes.txt\nLine: 3\nQuoted lines:\n> three\n"));
    let reviews = repo.reviews("steady").unwrap();
    assert_eq!(reviews.reviews.len(), 1, "{reviews:?}");
    assert_eq!(reviews.reviews[0].comments[0].body, "Why three?");
}
