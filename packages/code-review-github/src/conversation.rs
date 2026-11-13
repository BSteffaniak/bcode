//! Pull request conversations on GitHub: activity (state, reviews, threads, comments, checks)
//! through one batched GraphQL query, a watch that checks a cheap fingerprint of each pull
//! request, and the writes (replies, resolution, conversation comments, reviews) as GraphQL
//! mutations. Everything is mapped onto the provider-neutral conversation model; anchors are
//! reported as GitHub placed them, and the review maps them onto its own document.

use super::{Repository, ReviewSourceError, millis};
use bcode_code_review_models::{
    Check, CheckState, Checks, CommentPostRequest, CommentPostedResponse, DiffSide, HeldReply,
    Participant, ParticipantKind, PullActivity, PullLocator, PullReview, PullState, RemoteAnchor,
    RemoteComment, RemoteThread, ReviewBundle, ReviewBundleLine, ReviewBundleThread,
    ReviewDecision, ReviewLineKind, ReviewPreviewResponse, ReviewSourcePullsActivityRequest,
    ReviewSourcePullsActivityResponse, ReviewSourcePullsWatchRequest,
    ReviewSourcePullsWatchResponse, ReviewState, ReviewSubmitRequest, ReviewSubmitResponse,
    ReviewVerdict, ThreadReplyRequest, ThreadResolveRequest, ThreadResolveResponse,
    ViewerAbilities,
};
use bcode_code_review_provider::cancelled;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

/// The fields of a pull request's conversation.
const ACTIVITY_FIELDS: &str = "number title url state isDraft headRefOid body updatedAt \
    viewerDidAuthor author { __typename login url } reviewDecision \
    reviews(last: 100) { nodes { id state body submittedAt url author { __typename login url } \
    commit { oid } } } \
    reviewRequests(first: 50) { nodes { requestedReviewer { __typename \
    ... on User { login url } ... on Team { name slug url } ... on Bot { login url } \
    ... on Mannequin { login url } } } } \
    reviewThreads(first: 100) { nodes { id isResolved isOutdated viewerCanReply \
    viewerCanResolve viewerCanUnresolve path line startLine originalLine originalStartLine \
    diffSide startDiffSide subjectType opening: comments(first: 1) { nodes { diffHunk } } \
    comments(first: 100) { nodes { id body createdAt \
    updatedAt url author { __typename login url } pullRequestReview { id } \
    originalCommit { oid } } } } } \
    comments(last: 100) { nodes { id body createdAt updatedAt url \
    author { __typename login url } } } \
    commits(last: 1) { nodes { commit { oid statusCheckRollup { state contexts(first: 100) { \
    nodes { __typename ... on CheckRun { name conclusion status detailsUrl } \
    ... on StatusContext { context state targetUrl } } } } } } }";

/// Pull requests per activity request. Measured on a real repository: about one rate-limit
/// point per pull request, and about two seconds for ten; ten per request keeps each request
/// far inside GitHub's node limits (100 threads × 100 comments each) while the requests run in
/// parallel.
const ACTIVITY_PER_QUERY: usize = 10;

/// What the watch compares: anything a review, comment, reply, push, resolution, or check
/// changes. `updatedAt` moves with reviews, comments, and replies; resolution does not move it,
/// so each thread's state and comment count are included, and the head's check rollup for CI.
/// Reviews' and comments' own counts are left out: they made the query four times slower and
/// `updatedAt` already moves with them.
const FINGERPRINT_FIELDS: &str = "updatedAt headRefOid reviewDecision state isDraft \
    reviewThreads(first: 100) { nodes { isResolved comments { totalCount } } } \
    commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }";

/// Pull requests per fingerprint request. Measured on a real repository: one rate-limit point
/// per request of up to 50 pull requests (two for 100), about two seconds each. GitHub's
/// GraphQL API has no conditional requests, so every check costs a point per 50 pull requests:
/// at the default 30-second interval an 87-layer stack spends about 240 of the hourly 5,000.
const FINGERPRINT_PER_QUERY: usize = 50;

/// Default time between watch checks.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(30);
/// Shortest time between watch checks a configuration may ask for.
const MIN_INTERVAL: Duration = Duration::from_secs(10);
/// Longest time between checks while GitHub is unavailable.
const MAX_BACKOFF: Duration = Duration::from_secs(300);
/// How often a waiting watch looks at its cancellation.
const CANCEL_SLICE: Duration = Duration::from_millis(250);

/// Login GitHub shows for deleted accounts.
const GHOST: &str = "ghost";

pub fn get_activity(
    request: &ReviewSourcePullsActivityRequest,
) -> Result<ReviewSourcePullsActivityResponse, ReviewSourceError> {
    let repository = Repository::resolve(&request.repo_path, request.repository.as_deref())?;
    if cancelled() {
        return Err(ReviewSourceError::Cancelled);
    }
    let fields = repository
        .github(&request.repo_path)
        .pull_request_fields(
            &repository.0,
            &request.numbers,
            ACTIVITY_FIELDS,
            ACTIVITY_PER_QUERY,
        )
        .map_err(ReviewSourceError::from)?;
    let activities = request
        .numbers
        .iter()
        .filter_map(|number| {
            parse_activity(
                fields.pulls.get(number)?,
                fields.viewer.as_deref(),
                fields.viewer_permission.as_deref(),
            )
        })
        .collect();
    Ok(ReviewSourcePullsActivityResponse {
        repository: repository.display(),
        activities,
    })
}

/// One pull request's conversation from its [`ACTIVITY_FIELDS`]; `None` when it is unreadable.
pub fn parse_activity(
    node: &Value,
    viewer: Option<&str>,
    permission: Option<&str>,
) -> Option<PullActivity> {
    let state = match (
        node["state"].as_str()?,
        node["isDraft"].as_bool().unwrap_or(false),
    ) {
        ("MERGED", _) => PullState::Merged,
        ("CLOSED", _) => PullState::Closed,
        ("OPEN", true) => PullState::Draft,
        ("OPEN", false) => PullState::Open,
        _ => return None,
    };
    let decision = match node["reviewDecision"].as_str() {
        Some("APPROVED") => Some(ReviewDecision::Approved),
        Some("CHANGES_REQUESTED") => Some(ReviewDecision::ChangesRequested),
        Some("REVIEW_REQUIRED") => Some(ReviewDecision::ReviewRequired),
        _ => None,
    };
    let is_author = node["viewerDidAuthor"].as_bool().unwrap_or(false);
    // Anyone who can read a repository can comment on and review its pull requests; whether an
    // approval counts is the repository's rule, not the viewer's ability.
    let can_comment = permission.is_some();
    let open = matches!(state, PullState::Open | PullState::Draft);
    Some(PullActivity {
        number: node["number"].as_u64()?,
        title: text(&node["title"]).unwrap_or_default(),
        head_oid: text(&node["headRefOid"]).unwrap_or_default(),
        state,
        author: node["author"]
            .is_object()
            .then(|| participant(&node["author"], viewer)),
        description: text(&node["body"]).unwrap_or_default(),
        url: text(&node["url"]),
        decision,
        reviews: nodes(&node["reviews"])
            .filter_map(|review| parse_review(review, viewer))
            .collect(),
        requested: nodes(&node["reviewRequests"])
            .filter_map(|request| requested_reviewer(&request["requestedReviewer"], viewer))
            .collect(),
        threads: nodes(&node["reviewThreads"])
            .filter_map(|thread| parse_thread(thread, viewer))
            .collect(),
        comments: nodes(&node["comments"])
            .filter_map(|comment| parse_comment(comment, viewer))
            .collect(),
        checks: parse_checks(&node["commits"]["nodes"][0]["commit"]["statusCheckRollup"]),
        viewer: ViewerAbilities {
            is_author,
            can_review: can_comment && open,
            can_judge: can_comment && open && !is_author,
            can_comment,
        },
        updated_at_ms: node["updatedAt"].as_str().and_then(millis),
    })
}

fn text(value: &Value) -> Option<String> {
    value.as_str().map(str::to_owned)
}

/// The nodes of a GraphQL connection (none when it is missing).
fn nodes(connection: &Value) -> impl Iterator<Item = &Value> {
    connection["nodes"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
}

/// An author or actor (`null` for deleted accounts, shown as GitHub shows them).
fn participant(node: &Value, viewer: Option<&str>) -> Participant {
    let login = node["login"].as_str().unwrap_or(GHOST).to_owned();
    Participant {
        is_viewer: viewer.is_some_and(|viewer| viewer == login),
        kind: if node["__typename"].as_str() == Some("Bot") {
            ParticipantKind::Bot
        } else {
            ParticipantKind::User
        },
        url: text(&node["url"]),
        name: None,
        login,
    }
}

fn requested_reviewer(node: &Value, viewer: Option<&str>) -> Option<Participant> {
    match node["__typename"].as_str()? {
        "Team" => Some(Participant {
            login: text(&node["slug"])?,
            name: text(&node["name"]),
            kind: ParticipantKind::Team,
            url: text(&node["url"]),
            is_viewer: false,
        }),
        _ => node["login"].is_string().then(|| participant(node, viewer)),
    }
}

fn parse_review(node: &Value, viewer: Option<&str>) -> Option<PullReview> {
    let state = match node["state"].as_str()? {
        "APPROVED" => ReviewState::Approved,
        "CHANGES_REQUESTED" => ReviewState::ChangesRequested,
        "COMMENTED" => ReviewState::Commented,
        "DISMISSED" => ReviewState::Dismissed,
        "PENDING" => ReviewState::Pending,
        _ => return None,
    };
    Some(PullReview {
        id: text(&node["id"])?,
        author: participant(&node["author"], viewer),
        state,
        body: text(&node["body"]).unwrap_or_default(),
        at_ms: node["submittedAt"].as_str().and_then(millis),
        commit_oid: text(&node["commit"]["oid"]),
        url: text(&node["url"]),
    })
}

fn parse_comment(node: &Value, viewer: Option<&str>) -> Option<RemoteComment> {
    Some(RemoteComment {
        id: text(&node["id"])?,
        author: participant(&node["author"], viewer),
        body: text(&node["body"]).unwrap_or_default(),
        created_at_ms: node["createdAt"].as_str().and_then(millis),
        updated_at_ms: node["updatedAt"].as_str().and_then(millis),
        url: text(&node["url"]),
        review_id: text(&node["pullRequestReview"]["id"]),
    })
}

fn side(value: &Value) -> Option<DiffSide> {
    match value.as_str()? {
        "LEFT" => Some(DiffSide::Old),
        "RIGHT" => Some(DiffSide::New),
        _ => None,
    }
}

fn line(value: &Value) -> Option<u32> {
    value.as_u64().and_then(|line| u32::try_from(line).ok())
}

fn parse_thread(node: &Value, viewer: Option<&str>) -> Option<RemoteThread> {
    let comments: Vec<RemoteComment> = nodes(&node["comments"])
        .filter_map(|comment| parse_comment(comment, viewer))
        .collect();
    let resolved = node["isResolved"].as_bool().unwrap_or(false);
    let flag = |field: &str| node[field].as_bool().unwrap_or(false);
    Some(RemoteThread {
        id: text(&node["id"])?,
        anchor: RemoteAnchor {
            path: text(&node["path"])?,
            side: side(&node["diffSide"]).unwrap_or_default(),
            line: line(&node["line"]),
            start_line: line(&node["startLine"]),
            start_side: side(&node["startDiffSide"]),
            original_line: line(&node["originalLine"]),
            original_start_line: line(&node["originalStartLine"]),
            original_commit_oid: nodes(&node["comments"])
                .next()
                .and_then(|comment| text(&comment["originalCommit"]["oid"])),
            file_level: node["subjectType"].as_str() == Some("FILE"),
            diff_hunk: nodes(&node["opening"])
                .next()
                .and_then(|comment| text(&comment["diffHunk"]))
                .map(|hunk| bounded_hunk(&hunk)),
        },
        resolved,
        outdated: flag("isOutdated"),
        can_reply: flag("viewerCanReply"),
        can_resolve: if resolved {
            flag("viewerCanUnresolve")
        } else {
            flag("viewerCanResolve")
        },
        comments,
    })
}

/// Lines of a thread's diff hunk kept (the last ones: the hunk ends at the commented line).
const MAX_HUNK_LINES: usize = 40;
/// Bytes of a thread's diff hunk kept.
const MAX_HUNK_BYTES: usize = 4 * 1024;

/// The end of `hunk` within [`MAX_HUNK_LINES`] and [`MAX_HUNK_BYTES`], with a first line saying
/// how many lines were left out when any were.
fn bounded_hunk(hunk: &str) -> String {
    let lines: Vec<&str> = hunk.lines().collect();
    let mut kept: Vec<&str> = Vec::new();
    let mut bytes = 0;
    for line in lines.iter().rev() {
        if kept.len() == MAX_HUNK_LINES || bytes + line.len() + 1 > MAX_HUNK_BYTES {
            break;
        }
        bytes += line.len() + 1;
        kept.push(line);
    }
    kept.reverse();
    let omitted = lines.len() - kept.len();
    if omitted == 0 {
        return kept.join("\n");
    }
    format!(
        "… {omitted} earlier lines of the hunk left out\n{}",
        kept.join("\n")
    )
}

/// A check run's or status's state.
fn check_state(status: Option<&str>, conclusion: Option<&str>) -> CheckState {
    match (status, conclusion) {
        (Some(status), _) if status != "COMPLETED" => CheckState::Pending,
        (_, Some(conclusion)) => rollup_state(conclusion),
        _ => CheckState::Neutral,
    }
}

/// A rollup, conclusion, or status state.
fn rollup_state(state: &str) -> CheckState {
    match state {
        "SUCCESS" => CheckState::Passing,
        "FAILURE" | "ERROR" | "TIMED_OUT" | "ACTION_REQUIRED" | "STARTUP_FAILURE" => {
            CheckState::Failing
        }
        "QUEUED" | "IN_PROGRESS" | "PENDING" | "EXPECTED" | "WAITING" | "REQUESTED" => {
            CheckState::Pending
        }
        _ => CheckState::Neutral,
    }
}

fn parse_checks(rollup: &Value) -> Checks {
    let mut checks: Vec<Check> = nodes(&rollup["contexts"])
        .filter_map(|node| match node["__typename"].as_str()? {
            "CheckRun" => Some(Check {
                name: text(&node["name"])?,
                state: check_state(node["status"].as_str(), node["conclusion"].as_str()),
                url: text(&node["detailsUrl"]),
            }),
            "StatusContext" => Some(Check {
                name: text(&node["context"])?,
                state: rollup_state(node["state"].as_str()?),
                url: text(&node["targetUrl"]),
            }),
            _ => None,
        })
        .collect();
    checks.sort_by(|a, b| (a.state, &a.name).cmp(&(b.state, &b.name)));
    Checks {
        state: rollup["state"].as_str().map(rollup_state),
        checks,
    }
}

/// FNV-1a: a stable 64-bit hash, so a cursor means the same thing across processes.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Each pull request's fingerprint now.
fn fingerprints(
    repository: &Repository,
    request: &ReviewSourcePullsWatchRequest,
) -> Result<BTreeMap<u64, String>, ReviewSourceError> {
    let fields = repository
        .github(&request.repo_path)
        .pull_request_fields(
            &repository.0,
            &request.numbers,
            FINGERPRINT_FIELDS,
            FINGERPRINT_PER_QUERY,
        )
        .map_err(ReviewSourceError::from)?;
    Ok(fields
        .pulls
        .iter()
        .map(|(number, node)| {
            (
                *number,
                format!("{:016x}", fnv1a(node.to_string().as_bytes())),
            )
        })
        .collect())
}

/// Pull requests whose fingerprint differs from the one `before` recorded (pull requests
/// `before` did not record are new to the watch, not changed).
fn changed(before: &BTreeMap<u64, String>, now: &BTreeMap<u64, String>) -> Vec<u64> {
    before
        .iter()
        .filter(|(number, print)| now.get(number) != Some(*print))
        .map(|(number, _)| *number)
        .collect()
}

/// The configured interval between checks (`settings.interval_ms`), never below
/// [`MIN_INTERVAL`].
fn interval(settings: &Value) -> Duration {
    settings["interval_ms"]
        .as_u64()
        .map_or(DEFAULT_INTERVAL, Duration::from_millis)
        .max(MIN_INTERVAL)
}

/// Sleep `duration` (or until `deadline`), waking to look at the cancellation.
fn sleep(duration: Duration, deadline: Instant) -> Result<(), ReviewSourceError> {
    let until = (Instant::now() + duration).min(deadline);
    while Instant::now() < until {
        if cancelled() {
            return Err(ReviewSourceError::Cancelled);
        }
        std::thread::sleep(CANCEL_SLICE.min(until.saturating_duration_since(Instant::now())));
    }
    if cancelled() {
        return Err(ReviewSourceError::Cancelled);
    }
    Ok(())
}

pub fn watch_pulls(
    request: &ReviewSourcePullsWatchRequest,
) -> Result<ReviewSourcePullsWatchResponse, ReviewSourceError> {
    let repository = Repository::resolve(&request.repo_path, request.repository.as_deref())?;
    let deadline = Instant::now() + Duration::from_millis(request.wait_ms);
    let every = interval(&request.settings);
    let mut before: Option<BTreeMap<u64, String>> = request
        .cursor
        .as_deref()
        .and_then(|cursor| serde_json::from_str(cursor).ok());
    let mut backoff = every;
    loop {
        if cancelled() {
            return Err(ReviewSourceError::Cancelled);
        }
        match fingerprints(&repository, request) {
            Ok(now) => {
                backoff = every;
                let found = before.as_ref().map(|before| changed(before, &now));
                let mut cursor = before.take().unwrap_or_default();
                cursor.extend(now);
                if let Some(found) = found.filter(|found| !found.is_empty()) {
                    return Ok(watch_response(found, &cursor));
                }
                if Instant::now() + every >= deadline {
                    return Ok(watch_response(Vec::new(), &cursor));
                }
                before = Some(cursor);
                sleep(every, deadline)?;
            }
            // GitHub is down or limiting: wait longer each time, and report it when the wait
            // runs out still failing.
            Err(error @ ReviewSourceError::Unavailable(_)) => {
                if Instant::now() + backoff >= deadline {
                    return Err(error);
                }
                sleep(backoff, deadline)?;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
            Err(error) => return Err(error),
        }
    }
}

fn watch_response(
    changed: Vec<u64>,
    cursor: &BTreeMap<u64, String>,
) -> ReviewSourcePullsWatchResponse {
    ReviewSourcePullsWatchResponse {
        changed,
        cursor: serde_json::to_string(cursor).unwrap_or_default(),
    }
}

/// Run a GraphQL request against the repository's host and turn the errors it reports into
/// [`ReviewSourceError`]s.
fn graphql(
    repository: &Repository,
    pull: &PullLocator,
    query: &str,
    variables: Value,
) -> Result<Value, ReviewSourceError> {
    let value = repository
        .github(&pull.repo_path)
        .graphql(&repository.0.host, query, variables)?;
    bcode_forge::check_response(&value)?;
    Ok(value)
}

/// The pull request's node id and head commit.
fn pull_node(
    repository: &Repository,
    pull: &PullLocator,
) -> Result<(String, String), ReviewSourceError> {
    let value = graphql(
        repository,
        pull,
        "query($owner: String!, $name: String!, $number: Int!) { \
         repository(owner: $owner, name: $name) { pullRequest(number: $number) { id headRefOid } } }",
        json!({
            "owner": repository.0.owner,
            "name": repository.0.name,
            "number": pull.number,
        }),
    )?;
    let node = &value["data"]["repository"]["pullRequest"];
    match (text(&node["id"]), text(&node["headRefOid"])) {
        (Some(id), Some(head)) => Ok((id, head)),
        _ => Err(ReviewSourceError::PullNotFound(
            pull.number,
            repository.display(),
        )),
    }
}

/// The fields of a posted comment.
const COMMENT_FIELDS: &str =
    "id body createdAt updatedAt url author { __typename login url } pullRequestReview { id }";
/// The fields of a posted conversation comment (which belongs to no review).
const ISSUE_COMMENT_FIELDS: &str =
    "id body createdAt updatedAt url author { __typename login url }";

/// A comment the viewer just posted.
fn posted(node: &Value) -> Result<CommentPostedResponse, ReviewSourceError> {
    let mut comment = parse_comment(node, None)
        .ok_or_else(|| ReviewSourceError::Failed("GitHub did not return the comment".into()))?;
    comment.author.is_viewer = true;
    Ok(CommentPostedResponse { comment })
}

pub fn reply_to_thread(
    request: &ThreadReplyRequest,
) -> Result<CommentPostedResponse, ReviewSourceError> {
    let repository =
        Repository::resolve(&request.pull.repo_path, request.pull.repository.as_deref())?;
    let value = graphql(
        &repository,
        &request.pull,
        &format!(
            "mutation($thread: ID!, $body: String!) {{ addPullRequestReviewThreadReply(input: \
             {{ pullRequestReviewThreadId: $thread, body: $body }}) {{ comment {{ {COMMENT_FIELDS} }} }} }}"
        ),
        json!({ "thread": request.thread_id, "body": request.body }),
    )?;
    posted(&value["data"]["addPullRequestReviewThreadReply"]["comment"])
}

pub fn resolve_thread(
    request: &ThreadResolveRequest,
) -> Result<ThreadResolveResponse, ReviewSourceError> {
    let repository =
        Repository::resolve(&request.pull.repo_path, request.pull.repository.as_deref())?;
    let mutation = if request.resolved {
        "resolveReviewThread"
    } else {
        "unresolveReviewThread"
    };
    let value = graphql(
        &repository,
        &request.pull,
        &format!(
            "mutation($thread: ID!) {{ {mutation}(input: {{ threadId: $thread }}) \
             {{ thread {{ isResolved }} }} }}"
        ),
        json!({ "thread": request.thread_id }),
    )?;
    let resolved = value["data"][mutation]["thread"]["isResolved"]
        .as_bool()
        .ok_or_else(|| ReviewSourceError::Failed("GitHub did not return the thread".into()))?;
    Ok(ThreadResolveResponse { resolved })
}

pub fn post_comment(
    request: &CommentPostRequest,
) -> Result<CommentPostedResponse, ReviewSourceError> {
    let repository =
        Repository::resolve(&request.pull.repo_path, request.pull.repository.as_deref())?;
    let (subject, _) = pull_node(&repository, &request.pull)?;
    let value = graphql(
        &repository,
        &request.pull,
        &format!(
            "mutation($subject: ID!, $body: String!) {{ addComment(input: \
             {{ subjectId: $subject, body: $body }}) {{ commentEdge {{ node {{ {ISSUE_COMMENT_FIELDS} }} }} }} }}"
        ),
        json!({ "subject": subject, "body": request.body }),
    )?;
    posted(&value["data"]["addComment"]["commentEdge"]["node"])
}

/// One inline comment of a review, placed on a line of the pull request's diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineComment {
    /// File path.
    pub path: String,
    /// Body (markdown).
    pub body: String,
    /// The (last) line.
    pub line: u32,
    /// Its side.
    pub side: DiffSide,
    /// The first line of a range.
    pub start_line: Option<u32>,
    /// The first line's side.
    pub start_side: Option<DiffSide>,
}

/// A bundle's threads sorted into what goes inline and what goes into the review's body.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewDraft {
    /// Comments placed on diff lines.
    pub inline: Vec<InlineComment>,
    /// Comments that belong in the body by nature (on a file, on the whole stack, on context
    /// outside the diff), one line each.
    pub summarized: Vec<String>,
    /// Comments whose lines are not on the diff, one line each.
    pub unplaced: Vec<String>,
    /// Resolved threads, which are not published.
    pub skipped_resolved: usize,
}

/// Sort `bundle`'s threads into inline comments and body lines.
#[must_use]
pub fn review_draft(bundle: &ReviewBundle) -> ReviewDraft {
    let mut draft = ReviewDraft::default();
    for thread in &bundle.threads {
        if thread.resolved_at_ms.is_some() {
            draft.skipped_resolved += 1;
        } else if summarizes(thread) {
            draft.summarized.push(summary_line(thread));
        } else if let Some(comment) = inline_comment(thread) {
            draft.inline.push(comment);
        } else {
            draft.unplaced.push(summary_line(thread));
        }
    }
    draft
}

/// The review's body: `body`, then the comments that go into it.
#[must_use]
pub fn review_body(body: &str, draft: &ReviewDraft, unplaced_into_body: bool) -> String {
    let mut lines: Vec<&String> = draft.summarized.iter().collect();
    if unplaced_into_body {
        lines.extend(&draft.unplaced);
    }
    let mut out = body.trim().to_owned();
    if !lines.is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("#### Comments outside the diff\n\n");
        for line in lines {
            let _ = writeln!(out, "* {line}");
        }
    }
    out
}

/// Whether `thread` goes into the body rather than inline.
fn summarizes(thread: &ReviewBundleThread) -> bool {
    thread.summary_only
        || thread.anchor.is_file_anchor
        || thread
            .selected_lines
            .iter()
            .any(|line| line.surface_id.is_some() && line.kind == ReviewLineKind::Context)
}

fn summary_line(thread: &ReviewBundleThread) -> String {
    let line = thread
        .anchor
        .new_line
        .or(thread.anchor.new_start)
        .or(thread.anchor.old_line)
        .or(thread.anchor.old_start)
        .map_or_else(String::new, |line| format!(":{line}"));
    format!(
        "`{}{}`: {}",
        thread.anchor.file_path,
        line,
        thread_body(thread).replace('\n', " ")
    )
}

/// The side and number of a diff line.
const fn placed(line: &ReviewBundleLine) -> Option<(DiffSide, u32)> {
    match line.kind {
        ReviewLineKind::Added | ReviewLineKind::Context => match line.new_line {
            Some(number) => Some((DiffSide::New, number)),
            None => None,
        },
        ReviewLineKind::Removed => match line.old_line {
            Some(number) => Some((DiffSide::Old, number)),
            None => None,
        },
    }
}

fn inline_comment(thread: &ReviewBundleThread) -> Option<InlineComment> {
    let last = thread
        .selected_lines
        .iter()
        .rev()
        .find(|line| line.new_line.is_some() || line.old_line.is_some())?;
    let (side, number) = placed(last)?;
    // A range runs from the first selected line on the same side as its last line.
    let first = thread
        .selected_lines
        .iter()
        .filter_map(placed)
        .find(|(first_side, _)| *first_side == side)
        .map(|(_, first)| first)
        .filter(|first| *first != number);
    Some(InlineComment {
        path: last.file_path.clone(),
        body: thread_body(thread),
        line: number,
        side,
        start_line: first,
        start_side: first.map(|_| side),
    })
}

fn thread_body(thread: &ReviewBundleThread) -> String {
    thread
        .comments
        .iter()
        .map(|comment| comment.body.as_str())
        .collect::<Vec<_>>()
        .join("\n\n---\n\n")
}

const fn side_name(side: DiffSide) -> &'static str {
    match side {
        DiffSide::Old => "LEFT",
        DiffSide::New => "RIGHT",
    }
}

const fn event(verdict: ReviewVerdict) -> &'static str {
    match verdict {
        ReviewVerdict::Comment => "COMMENT",
        ReviewVerdict::Approve => "APPROVE",
        ReviewVerdict::RequestChanges => "REQUEST_CHANGES",
    }
}

const fn verdict_label(verdict: ReviewVerdict) -> &'static str {
    match verdict {
        ReviewVerdict::Comment => "Comment",
        ReviewVerdict::Approve => "Approve",
        ReviewVerdict::RequestChanges => "Request changes",
    }
}

/// The draft of `request`, failing when comments cannot be placed and may not go into the body.
fn checked_draft(request: &ReviewSubmitRequest) -> Result<ReviewDraft, ReviewSourceError> {
    let draft = request
        .bundle
        .as_ref()
        .map(review_draft)
        .unwrap_or_default();
    if !request.unplaced_into_body && !draft.unplaced.is_empty() {
        return Err(ReviewSourceError::InvalidRequest(format!(
            "{} comment{} cannot be placed on the diff: {}",
            draft.unplaced.len(),
            if draft.unplaced.len() == 1 { "" } else { "s" },
            draft.unplaced.join("; ")
        )));
    }
    Ok(draft)
}

/// What submitting `request` would send, for a person to read. Local: no request is made.
pub fn preview_review(
    request: &ReviewSubmitRequest,
) -> Result<ReviewPreviewResponse, ReviewSourceError> {
    let repository =
        Repository::resolve(&request.pull.repo_path, request.pull.repository.as_deref())?;
    let draft = request
        .bundle
        .as_ref()
        .map(review_draft)
        .unwrap_or_default();
    let body = review_body(&request.body, &draft, request.unplaced_into_body);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# {} on {}#{}\n",
        verdict_label(request.verdict),
        repository.display(),
        request.pull.number
    );
    if let Some(head) = &request.head_oid {
        let _ = writeln!(out, "* Reviewed at `{head}`");
    }
    let _ = writeln!(out, "* Inline comments: {}", draft.inline.len());
    let _ = writeln!(out, "* Replies to threads: {}", request.replies.len());
    if draft.skipped_resolved > 0 {
        let _ = writeln!(
            out,
            "* Resolved threads left out: {}",
            draft.skipped_resolved
        );
    }
    if !draft.unplaced.is_empty() {
        let _ = writeln!(
            out,
            "* Not on the diff: {}{}",
            draft.unplaced.len(),
            if request.unplaced_into_body {
                " (in the body)"
            } else {
                " (submitting will fail)"
            }
        );
    }
    if !body.is_empty() {
        let _ = write!(out, "\n## Body\n\n{body}\n");
    }
    if !draft.inline.is_empty() {
        out.push_str("\n## Inline comments\n\n");
        for comment in &draft.inline {
            let lines = comment.start_line.map_or_else(
                || comment.line.to_string(),
                |start| format!("{start}-{}", comment.line),
            );
            let _ = write!(
                out,
                "* `{}` line {lines} ({})\n\n{}\n\n",
                comment.path,
                match comment.side {
                    DiffSide::Old => "old",
                    DiffSide::New => "new",
                },
                comment.body
            );
        }
    }
    if !request.replies.is_empty() {
        out.push_str("\n## Replies\n\n");
        for reply in &request.replies {
            let _ = write!(out, "* {}\n\n", reply.body);
        }
    }
    Ok(ReviewPreviewResponse {
        preview: out,
        unplaced: draft.unplaced.len(),
    })
}

/// The `threads` input of `addPullRequestReview`.
fn draft_threads(draft: &ReviewDraft) -> Value {
    Value::Array(
        draft
            .inline
            .iter()
            .map(|comment| {
                let mut thread = json!({
                    "path": comment.path,
                    "body": comment.body,
                    "line": comment.line,
                    "side": side_name(comment.side),
                });
                if let (Some(start), Some(start_side)) = (comment.start_line, comment.start_side) {
                    thread["startLine"] = json!(start);
                    thread["startSide"] = json!(side_name(start_side));
                }
                thread
            })
            .collect(),
    )
}

pub fn submit_review(
    request: &ReviewSubmitRequest,
) -> Result<ReviewSubmitResponse, ReviewSourceError> {
    let repository =
        Repository::resolve(&request.pull.repo_path, request.pull.repository.as_deref())?;
    let draft = checked_draft(request)?;
    let body = review_body(&request.body, &draft, request.unplaced_into_body);
    let (pull_id, head) = pull_node(&repository, &request.pull)?;
    let moved = request
        .head_oid
        .as_deref()
        .is_some_and(|reviewed| reviewed != head);
    let stale = |error: ReviewSourceError| match error {
        // The reviewed commit is no longer part of the pull request: its lines mean nothing now.
        ReviewSourceError::Failed(message) if moved => ReviewSourceError::Stale(format!(
            "#{} moved since it was reviewed (now at {head}): {message}",
            request.pull.number
        )),
        other => other,
    };
    let mut input = json!({
        "pullRequestId": pull_id,
        "threads": draft_threads(&draft),
    });
    if let Some(reviewed) = &request.head_oid {
        input["commitOID"] = json!(reviewed);
    }
    let review = if request.replies.is_empty() {
        input["event"] = json!(event(request.verdict));
        input["body"] = json!(body);
        add_review(&repository, &request.pull, &input).map_err(stale)?
    } else {
        // Replies join a pending review, which is then submitted; a failure on the way deletes
        // it so no half-made review is left behind.
        let pending = add_review(&repository, &request.pull, &input).map_err(stale)?;
        let finished = add_replies(&repository, &request.pull, &pending.0, &request.replies)
            .and_then(|()| {
                submit_pending(
                    &repository,
                    &request.pull,
                    &pending.0,
                    request.verdict,
                    &body,
                )
            });
        if let Err(error) = finished {
            let _ = delete_pending(&repository, &request.pull, &pending.0);
            return Err(error);
        }
        pending
    };
    Ok(ReviewSubmitResponse {
        url: review.1,
        message: format!(
            "{} on #{}: {} inline comment{}{}",
            verdict_label(request.verdict),
            request.pull.number,
            draft.inline.len(),
            if draft.inline.len() == 1 { "" } else { "s" },
            if request.replies.is_empty() {
                String::new()
            } else {
                format!(
                    ", {} repl{}",
                    request.replies.len(),
                    if request.replies.len() == 1 {
                        "y"
                    } else {
                        "ies"
                    }
                )
            }
        ),
    })
}

/// Create a review from `input`; its id and web page.
fn add_review(
    repository: &Repository,
    pull: &PullLocator,
    input: &Value,
) -> Result<(String, Option<String>), ReviewSourceError> {
    let value = graphql(
        repository,
        pull,
        "mutation($input: AddPullRequestReviewInput!) { addPullRequestReview(input: $input) \
         { pullRequestReview { id url } } }",
        json!({ "input": input }),
    )?;
    let review = &value["data"]["addPullRequestReview"]["pullRequestReview"];
    let id = text(&review["id"])
        .ok_or_else(|| ReviewSourceError::Failed("GitHub did not return the review".into()))?;
    Ok((id, text(&review["url"])))
}

fn add_replies(
    repository: &Repository,
    pull: &PullLocator,
    review: &str,
    replies: &[HeldReply],
) -> Result<(), ReviewSourceError> {
    for reply in replies {
        graphql(
            repository,
            pull,
            "mutation($review: ID!, $thread: ID!, $body: String!) { \
             addPullRequestReviewThreadReply(input: { pullRequestReviewId: $review, \
             pullRequestReviewThreadId: $thread, body: $body }) { comment { id } } }",
            json!({ "review": review, "thread": reply.thread_id, "body": reply.body }),
        )?;
    }
    Ok(())
}

fn submit_pending(
    repository: &Repository,
    pull: &PullLocator,
    review: &str,
    verdict: ReviewVerdict,
    body: &str,
) -> Result<(), ReviewSourceError> {
    graphql(
        repository,
        pull,
        "mutation($review: ID!, $event: PullRequestReviewEvent!, $body: String) { \
         submitPullRequestReview(input: { pullRequestReviewId: $review, event: $event, \
         body: $body }) { pullRequestReview { id } } }",
        json!({ "review": review, "event": event(verdict), "body": body }),
    )
    .map(|_| ())
}

fn delete_pending(
    repository: &Repository,
    pull: &PullLocator,
    review: &str,
) -> Result<(), ReviewSourceError> {
    graphql(
        repository,
        pull,
        "mutation($review: ID!) { deletePullRequestReview(input: { pullRequestReviewId: $review }) \
         { pullRequestReview { id } } }",
        json!({ "review": review }),
    )
    .map(|_| ())
}

#[cfg(test)]
mod tests;
