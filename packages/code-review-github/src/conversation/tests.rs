//! Conversation tests: GitHub's answers read into the neutral model, the watch's comparison,
//! and reviews drafted from bundles (no network).

use super::*;
use bcode_code_review_models::{
    DraftAnchor, DraftComment, ReviewAnchorKind, ReviewTarget, ReviewThreadKind,
    ReviewThreadSeverity,
};
use std::path::PathBuf;

/// A pull request as the activity query answers it.
fn activity_node() -> Value {
    serde_json::from_str(
        r#"{
        "number": 7,
        "title": "Add the parser",
        "url": "https://github.com/o/r/pull/7",
        "state": "OPEN",
        "isDraft": false,
        "headRefOid": "abc",
        "body": "Adds it.",
        "updatedAt": "2026-11-12T19:47:04Z",
        "viewerDidAuthor": false,
        "author": { "__typename": "User", "login": "ana", "url": "https://github.com/ana" },
        "reviewDecision": "CHANGES_REQUESTED",
        "reviews": { "nodes": [
            { "id": "R1", "state": "APPROVED", "body": "", "submittedAt": "2026-11-10T00:00:00Z",
              "url": "u1", "author": { "__typename": "User", "login": "me" },
              "commit": { "oid": "old" } },
            { "id": "R2", "state": "CHANGES_REQUESTED", "body": "Please fix",
              "submittedAt": "2026-11-11T00:00:00Z", "url": "u2",
              "author": { "__typename": "Bot", "login": "lint-bot" }, "commit": { "oid": "abc" } },
            { "id": "R3", "state": "SOMETHING_NEW", "author": null }
        ] },
        "reviewRequests": { "nodes": [
            { "requestedReviewer": { "__typename": "User", "login": "raj", "url": "r" } },
            { "requestedReviewer": { "__typename": "Team", "name": "Core", "slug": "core",
              "url": "t" } },
            { "requestedReviewer": null }
        ] },
        "reviewThreads": { "nodes": [
            { "id": "T1", "isResolved": false, "isOutdated": false, "viewerCanReply": true,
              "viewerCanResolve": true, "viewerCanUnresolve": false, "path": "src/a.rs",
              "line": 12, "startLine": 10, "originalLine": 11, "originalStartLine": 9,
              "diffSide": "RIGHT", "startDiffSide": "RIGHT", "subjectType": "LINE",
              "comments": { "nodes": [
                  { "id": "C1", "body": "Why?", "createdAt": "2026-11-11T01:00:00Z",
                    "updatedAt": "2026-11-11T01:00:00Z", "url": "c1",
                    "author": { "__typename": "User", "login": "raj" },
                    "pullRequestReview": { "id": "R2" }, "originalCommit": { "oid": "first" } },
                  { "id": "C2", "body": "Because.", "createdAt": "2026-11-11T02:00:00Z",
                    "updatedAt": "2026-11-11T02:00:00Z", "url": "c2", "author": null,
                    "pullRequestReview": null, "originalCommit": { "oid": "abc" } }
              ] } },
            { "id": "T2", "isResolved": true, "isOutdated": true, "viewerCanReply": true,
              "viewerCanResolve": false, "viewerCanUnresolve": true, "path": "README.md",
              "line": null, "startLine": null, "originalLine": 3, "originalStartLine": null,
              "diffSide": "LEFT", "startDiffSide": null, "subjectType": "FILE",
              "comments": { "nodes": [] } }
        ] },
        "comments": { "nodes": [
            { "id": "I1", "body": "Looks good overall", "createdAt": "2026-11-12T00:00:00Z",
              "updatedAt": "2026-11-12T03:00:00Z", "url": "i1",
              "author": { "__typename": "User", "login": "me" } }
        ] },
        "commits": { "nodes": [ { "commit": { "oid": "abc", "statusCheckRollup": {
            "state": "FAILURE",
            "contexts": { "nodes": [
                { "__typename": "CheckRun", "name": "test", "conclusion": "SUCCESS",
                  "status": "COMPLETED", "detailsUrl": "d1" },
                { "__typename": "CheckRun", "name": "lint", "conclusion": "FAILURE",
                  "status": "COMPLETED", "detailsUrl": "d2" },
                { "__typename": "CheckRun", "name": "build", "conclusion": null,
                  "status": "IN_PROGRESS", "detailsUrl": "d3" },
                { "__typename": "StatusContext", "context": "deploy", "state": "PENDING",
                  "targetUrl": "d4" },
                { "__typename": "CheckRun", "name": "docs", "conclusion": "SKIPPED",
                  "status": "COMPLETED", "detailsUrl": null }
            ] } } } } ] }
    }"#,
    )
    .expect("fixture")
}

#[test]
fn reads_a_pull_requests_conversation() {
    let activity = parse_activity(&activity_node(), Some("me"), Some("WRITE")).expect("activity");
    assert_eq!(activity.number, 7);
    assert_eq!(activity.state, PullState::Open);
    assert_eq!(activity.decision, Some(ReviewDecision::ChangesRequested));
    assert_eq!(activity.description, "Adds it.");
    assert_eq!(activity.updated_at_ms, millis("2026-11-12T19:47:04Z"));
    assert_eq!(
        activity.viewer,
        ViewerAbilities {
            is_author: false,
            can_review: true,
            can_judge: true,
            can_comment: true,
        }
    );

    // Reviews: an unknown state is left out; the viewer and bots are recognized.
    let reviews: Vec<(&str, ReviewState, bool, ParticipantKind)> = activity
        .reviews
        .iter()
        .map(|review| {
            (
                review.id.as_str(),
                review.state,
                review.author.is_viewer,
                review.author.kind,
            )
        })
        .collect();
    assert_eq!(
        reviews,
        vec![
            ("R1", ReviewState::Approved, true, ParticipantKind::User),
            (
                "R2",
                ReviewState::ChangesRequested,
                false,
                ParticipantKind::Bot
            ),
        ]
    );
    assert_eq!(activity.reviews[0].commit_oid.as_deref(), Some("old"));

    // Requested reviewers: people and teams.
    let requested: Vec<(&str, ParticipantKind)> = activity
        .requested
        .iter()
        .map(|participant| (participant.login.as_str(), participant.kind))
        .collect();
    assert_eq!(
        requested,
        vec![
            ("raj", ParticipantKind::User),
            ("core", ParticipantKind::Team)
        ]
    );
    assert_eq!(activity.requested[1].name.as_deref(), Some("Core"));

    // Threads: anchors as GitHub placed them; resolving means unresolving once resolved.
    let open = &activity.threads[0];
    assert_eq!(
        open.anchor,
        RemoteAnchor {
            path: "src/a.rs".into(),
            side: DiffSide::New,
            line: Some(12),
            start_line: Some(10),
            start_side: Some(DiffSide::New),
            original_line: Some(11),
            original_start_line: Some(9),
            original_commit_oid: Some("first".into()),
            file_level: false,
        }
    );
    assert!(open.can_reply && open.can_resolve && !open.resolved && !open.outdated);
    assert_eq!(open.comments.len(), 2);
    assert_eq!(open.comments[0].review_id.as_deref(), Some("R2"));
    assert_eq!(open.comments[1].author.login, GHOST);
    let resolved = &activity.threads[1];
    assert!(resolved.resolved && resolved.outdated && resolved.can_resolve);
    assert!(resolved.anchor.file_level);
    assert_eq!(resolved.anchor.side, DiffSide::Old);
    assert_eq!(resolved.anchor.line, None);

    // Conversation comments.
    assert_eq!(activity.comments.len(), 1);
    assert!(activity.comments[0].author.is_viewer);

    // Checks: failing first, then pending, neutral, passing.
    assert_eq!(activity.checks.state, Some(CheckState::Failing));
    let checks: Vec<(&str, CheckState)> = activity
        .checks
        .checks
        .iter()
        .map(|check| (check.name.as_str(), check.state))
        .collect();
    assert_eq!(
        checks,
        vec![
            ("lint", CheckState::Failing),
            ("build", CheckState::Pending),
            ("deploy", CheckState::Pending),
            ("docs", CheckState::Neutral),
            ("test", CheckState::Passing),
        ]
    );
}

#[test]
fn the_author_cannot_judge_and_closed_pull_requests_take_no_reviews() {
    let mut node = activity_node();
    node["viewerDidAuthor"] = json!(true);
    let mine = parse_activity(&node, Some("ana"), Some("WRITE")).expect("activity");
    assert!(mine.viewer.is_author && mine.viewer.can_review && !mine.viewer.can_judge);

    node["state"] = json!("MERGED");
    let merged = parse_activity(&node, Some("ana"), Some("WRITE")).expect("activity");
    assert_eq!(merged.state, PullState::Merged);
    assert!(merged.viewer.can_comment && !merged.viewer.can_review);

    node["state"] = json!("OPEN");
    node["isDraft"] = json!(true);
    let draft = parse_activity(&node, None, None).expect("activity");
    assert_eq!(draft.state, PullState::Draft);
    assert_eq!(
        draft.viewer,
        ViewerAbilities {
            is_author: true,
            ..ViewerAbilities::default()
        }
    );
}

#[test]
fn an_unreadable_pull_request_is_left_out() {
    let mut node = activity_node();
    node["state"] = json!("UNKNOWN");
    assert!(parse_activity(&node, None, None).is_none());
}

#[test]
fn the_watch_reports_changed_pull_requests_only() {
    let print = |entries: &[(u64, &str)]| -> BTreeMap<u64, String> {
        entries
            .iter()
            .map(|(number, print)| (*number, (*print).to_owned()))
            .collect()
    };
    let before = print(&[(1, "a"), (2, "b"), (3, "c")]);
    // 2 changed, 3 is gone (closed or deleted), 4 is new to the watch.
    let now = print(&[(1, "a"), (2, "x"), (4, "d")]);
    assert_eq!(changed(&before, &now), vec![2, 3]);
    assert_eq!(changed(&before, &before), Vec::<u64>::new());

    let cursor = watch_response(vec![2], &now).cursor;
    let read: BTreeMap<u64, String> = serde_json::from_str(&cursor).expect("cursor");
    assert_eq!(read, now);
}

#[test]
fn fingerprints_are_stable_and_intervals_have_a_floor() {
    assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
    assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    assert_eq!(interval(&Value::Null), DEFAULT_INTERVAL);
    assert_eq!(interval(&json!({ "interval_ms": 1 })), MIN_INTERVAL);
    assert_eq!(
        interval(&json!({ "interval_ms": 60_000 })),
        Duration::from_secs(60)
    );
}

#[test]
fn a_cancelled_watch_stops_waiting() {
    let cancel = bcode_code_review_provider::Cancel::default();
    cancel.cancel();
    let deadline = Instant::now() + Duration::from_secs(60);
    let result = bcode_code_review_provider::with_cancel(&cancel, || {
        sleep(Duration::from_secs(60), deadline)
    });
    assert!(matches!(result, Err(ReviewSourceError::Cancelled)));
}

fn diff_line(kind: ReviewLineKind, old: Option<u32>, new: Option<u32>) -> ReviewBundleLine {
    ReviewBundleLine {
        file_path: "src/a.rs".into(),
        kind,
        old_line: old,
        new_line: new,
        diff_row: 0,
        content: String::new(),
        surface_id: None,
        source_id: None,
    }
}

fn anchor() -> DraftAnchor {
    DraftAnchor {
        kind: ReviewAnchorKind::Range,
        file_path: "src/a.rs".into(),
        diff_row: 0,
        start_diff_row: None,
        end_diff_row: None,
        old_start: None,
        old_end: None,
        new_start: None,
        new_end: None,
        old_line: None,
        new_line: Some(4),
        line_kind: ReviewLineKind::Added,
        is_file_anchor: false,
        surface_id: None,
        source_id: None,
        head_oid: None,
        blob_oid: None,
        reply_to: None,
    }
}

fn thread(id: &str, lines: Vec<ReviewBundleLine>, body: &str) -> ReviewBundleThread {
    ReviewBundleThread {
        thread_id: id.into(),
        anchor: anchor(),
        comments: vec![DraftComment {
            comment_id: format!("{id}c"),
            thread_id: id.into(),
            anchor: anchor(),
            body: body.into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            session_id: None,
            resolved_at_ms: None,
            thread_kind: ReviewThreadKind::Note,
            severity: ReviewThreadSeverity::Info,
        }],
        session_id: None,
        resolved_at_ms: None,
        selected_lines: lines,
        selected_diff_lines: Vec::new(),
        hunk_context: Vec::new(),
        summary_only: false,
    }
}

fn bundle(threads: Vec<ReviewBundleThread>) -> ReviewBundle {
    ReviewBundle {
        review_id: "r".into(),
        title: "t".into(),
        repo_root: PathBuf::from("/repo"),
        target: ReviewTarget::WorkingTreeUnstaged,
        files: Vec::new(),
        surfaces: Vec::new(),
        threads,
        generated_at_ms: 0,
        pull_request: None,
    }
}

#[test]
fn drafts_sort_threads_into_inline_comments_and_body_lines() {
    let range = thread(
        "range",
        vec![
            diff_line(ReviewLineKind::Context, Some(1), Some(1)),
            diff_line(ReviewLineKind::Added, None, Some(2)),
            diff_line(ReviewLineKind::Added, None, Some(3)),
        ],
        "A range",
    );
    let removed = thread(
        "removed",
        vec![diff_line(ReviewLineKind::Removed, Some(9), None)],
        "Gone",
    );
    let nowhere = thread("nowhere", Vec::new(), "No lines");
    let mut file = thread("file", Vec::new(), "On the file");
    file.anchor.is_file_anchor = true;
    let mut whole = thread(
        "whole",
        vec![diff_line(ReviewLineKind::Added, None, Some(1))],
        "Stack",
    );
    whole.summary_only = true;
    let mut resolved = thread(
        "done",
        vec![diff_line(ReviewLineKind::Added, None, Some(1))],
        "x",
    );
    resolved.resolved_at_ms = Some(1);

    let draft = review_draft(&bundle(vec![
        range, removed, nowhere, file, whole, resolved,
    ]));
    assert_eq!(
        draft.inline,
        vec![
            InlineComment {
                path: "src/a.rs".into(),
                body: "A range".into(),
                line: 3,
                side: DiffSide::New,
                start_line: Some(1),
                start_side: Some(DiffSide::New),
            },
            InlineComment {
                path: "src/a.rs".into(),
                body: "Gone".into(),
                line: 9,
                side: DiffSide::Old,
                start_line: None,
                start_side: None,
            },
        ]
    );
    assert_eq!(
        draft.summarized,
        vec!["`src/a.rs:4`: On the file", "`src/a.rs:4`: Stack"]
    );
    assert_eq!(draft.unplaced, vec!["`src/a.rs:4`: No lines"]);
    assert_eq!(draft.skipped_resolved, 1);

    assert_eq!(
        review_body("Summary", &draft, true),
        "Summary\n\n#### Comments outside the diff\n\n* `src/a.rs:4`: On the file\n\
         * `src/a.rs:4`: Stack\n* `src/a.rs:4`: No lines\n"
    );
    assert_eq!(review_body("", &ReviewDraft::default(), true), "");
    let threads = draft_threads(&draft);
    assert_eq!(
        threads[0],
        json!({ "path": "src/a.rs", "body": "A range", "line": 3, "side": "RIGHT",
                "startLine": 1, "startSide": "RIGHT" })
    );
    assert_eq!(
        threads[1],
        json!({ "path": "src/a.rs", "body": "Gone", "line": 9, "side": "LEFT" })
    );
}

fn submit_request(bundle: Option<ReviewBundle>, into_body: bool) -> ReviewSubmitRequest {
    ReviewSubmitRequest {
        pull: PullLocator {
            repo_path: PathBuf::from("/nonexistent"),
            repository: Some("o/r".into()),
            number: 7,
        },
        verdict: ReviewVerdict::RequestChanges,
        body: "Needs work".into(),
        head_oid: Some("abc".into()),
        bundle,
        replies: vec![HeldReply {
            thread_id: "T1".into(),
            body: "Agreed".into(),
        }],
        unplaced_into_body: into_body,
    }
}

#[test]
fn the_preview_is_local_and_says_what_would_be_sent() {
    let request = submit_request(
        Some(bundle(vec![
            thread(
                "a",
                vec![diff_line(ReviewLineKind::Added, None, Some(2))],
                "Inline",
            ),
            thread("b", Vec::new(), "Lost"),
        ])),
        true,
    );
    let preview = preview_review(&request).expect("preview");
    assert_eq!(preview.unplaced, 1);
    assert!(preview.preview.starts_with("# Request changes on o/r#7\n"));
    assert!(preview.preview.contains("* Inline comments: 1"));
    assert!(preview.preview.contains("* Replies to threads: 1"));
    assert!(
        preview
            .preview
            .contains("* Not on the diff: 1 (in the body)")
    );
    assert!(preview.preview.contains("Needs work"));
    assert!(preview.preview.contains("`src/a.rs` line 2 (new)"));
    assert!(preview.preview.contains("* Agreed"));
}

#[test]
fn comments_that_cannot_be_placed_fail_a_submission_that_keeps_them_out_of_the_body() {
    let request = submit_request(Some(bundle(vec![thread("b", Vec::new(), "Lost")])), false);
    let error = checked_draft(&request).expect_err("unplaced");
    assert_eq!(error.code(), "invalid_request");
    assert!(error.to_string().contains("Lost"));
    assert!(checked_draft(&submit_request(None, false)).is_ok());
}

#[test]
fn verdicts_map_to_github_events() {
    assert_eq!(event(ReviewVerdict::Comment), "COMMENT");
    assert_eq!(event(ReviewVerdict::Approve), "APPROVE");
    assert_eq!(event(ReviewVerdict::RequestChanges), "REQUEST_CHANGES");
}

/// The pull request `BCODE_REVIEW_LIVE_PULL` names (`owner/name#number`), read live with
/// read-only requests; skipped when it is unset.
fn live_pull() -> Option<(String, u64)> {
    let named = std::env::var("BCODE_REVIEW_LIVE_PULL").ok()?;
    let (repository, number) = named.split_once('#')?;
    Some((repository.to_owned(), number.parse().ok()?))
}

#[test]
#[ignore = "reads GitHub; set BCODE_REVIEW_LIVE_PULL=owner/name#number"]
fn live_activity_and_an_unchanged_watch() {
    let Some((repository, number)) = live_pull() else {
        return;
    };
    let repo_path = std::env::current_dir().expect("cwd");
    let activity = get_activity(&ReviewSourcePullsActivityRequest {
        repo_path: repo_path.clone(),
        repository: Some(repository.clone()),
        numbers: vec![number],
    })
    .expect("activity");
    let pull = &activity.activities[0];
    eprintln!(
        "#{} {:?} {:?}: {} reviews, {} threads, {} comments, {} checks, viewer {:?}",
        pull.number,
        pull.state,
        pull.decision,
        pull.reviews.len(),
        pull.threads.len(),
        pull.comments.len(),
        pull.checks.checks.len(),
        pull.viewer
    );
    let request = |cursor: Option<String>| ReviewSourcePullsWatchRequest {
        repo_path: repo_path.clone(),
        repository: Some(repository.clone()),
        numbers: vec![number],
        cursor,
        wait_ms: 0,
        settings: Value::Null,
    };
    let first = watch_pulls(&request(None)).expect("baseline");
    assert_eq!(first.changed, [] as [u64; 0]);
    let second = watch_pulls(&request(Some(first.cursor.clone()))).expect("check");
    assert_eq!(second.changed, [] as [u64; 0], "nothing changed in between");
    assert_eq!(second.cursor, first.cursor);
}
