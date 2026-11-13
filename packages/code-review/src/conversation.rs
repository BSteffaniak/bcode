//! Pull request conversations in a review: reading, placing threads, judging reviews.
//!
//! Reading them from providers (cached), placing their inline threads on the review's current
//! code, and judging whether a review still applies to a layer's current version.
//! Provider-neutral: every provider reports the same [`PullActivity`], and placement is done
//! here, the same way for all of them. See `docs/review-forge-conversation.md`.
//!
//! * [`cached_activity`] answers from the on-disk cache at once; [`fetch_activity`] asks the
//!   provider (one call for every pull request) and refreshes the cache.
//! * [`place_threads`] maps each thread onto a layer's current head: exactly when the forge's
//!   diff is the review's, else carried through `git diff` from the commit the forge placed it
//!   on, else attached to its file (or, when the layer no longer changes the file, to the
//!   layer). One `git diff` per pair of commits, whatever the number of threads.
//! * [`review_currency`] tells whether a review was given on the current change, using stack
//!   history: a layer that was only rebased since keeps its reviews current.

use crate::data::{DataError, git};
use crate::provider::{ProviderCallError, ProviderRegistry, ProviderResult};
use bcode_code_review_models::{
    ChangeKind, DiffSide, DraftAnchor, LayerTimeline, PullActivity, RemoteThread, ReviewAnchorKind,
    ReviewLineKind, ReviewSourcePullsActivityRequest, ReviewSourcePullsWatchRequest,
    ReviewSourcePullsWatchResponse,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Identifies one pull request's conversation in the cache.
fn cache_key(provider: &str, repository: &str, number: u64) -> String {
    format!("{provider}\u{1}{repository}\u{1}{number}")
}

/// The conversations of `numbers` the cache holds, with when each was read.
#[must_use]
pub fn cached_activity(
    registry: &ProviderRegistry,
    provider: &str,
    repository: &str,
    numbers: &[u64],
) -> BTreeMap<u64, (PullActivity, std::time::SystemTime)> {
    let keys: Vec<String> = numbers
        .iter()
        .map(|number| cache_key(provider, repository, *number))
        .collect();
    let entries = registry
        .disk()
        .get_many(crate::cache::CacheKind::Activity, &keys);
    numbers
        .iter()
        .zip(&keys)
        .filter_map(|(number, key)| {
            let entry = entries.get(key)?;
            let activity: PullActivity = serde_json::from_str(&entry.value).ok()?;
            Some((*number, (activity, entry.written)))
        })
        .collect()
}

/// Whether `provider` reports conversations.
#[must_use]
pub fn reports_activity(registry: &ProviderRegistry, provider: &str) -> bool {
    registry
        .source(provider)
        .is_some_and(|route| route.manifest.conversation.activity)
}

/// The conversations of `numbers` as the provider reports them now (and kept in the cache).
///
/// # Errors
/// The provider's failure; `unsupported_operation` when it does not report conversations.
pub fn fetch_activity(
    registry: &ProviderRegistry,
    repo_path: &Path,
    provider: &str,
    repository: &str,
    numbers: &[u64],
) -> ProviderResult<Vec<PullActivity>> {
    let route = registry
        .source(provider)
        .filter(|route| route.manifest.conversation.activity)
        .ok_or_else(|| {
            ProviderCallError::new(
                bcode_code_review_provider::ERROR_UNSUPPORTED_OPERATION,
                format!("{provider} does not report pull request conversations"),
            )
        })?;
    let answer = route
        .provider
        .get_activity(&ReviewSourcePullsActivityRequest {
            repo_path: repo_path.to_path_buf(),
            repository: Some(repository.to_owned()),
            numbers: numbers.to_vec(),
        })?;
    let entries: Vec<(String, String)> = answer
        .activities
        .iter()
        .filter_map(|activity| {
            Some((
                cache_key(provider, repository, activity.number),
                serde_json::to_string(activity).ok()?,
            ))
        })
        .collect();
    registry
        .disk()
        .put_many(crate::cache::CacheKind::Activity, &entries);
    Ok(answer.activities)
}

/// Whether `provider` can say when conversations change.
#[must_use]
pub fn watches(registry: &ProviderRegistry, provider: &str) -> bool {
    registry
        .source(provider)
        .is_some_and(|route| route.manifest.conversation.watch.is_some())
}

/// Wait for the provider to report changed conversations (see
/// [`bcode_code_review_provider::ReviewSourceProvider::watch_pulls`]).
///
/// # Errors
/// The provider's failure; `unsupported_operation` when it cannot watch.
pub fn watch(
    registry: &ProviderRegistry,
    provider: &str,
    request: &ReviewSourcePullsWatchRequest,
) -> ProviderResult<ReviewSourcePullsWatchResponse> {
    let route = registry
        .source(provider)
        .filter(|route| route.manifest.conversation.watch.is_some())
        .ok_or_else(|| {
            ProviderCallError::new(
                bcode_code_review_provider::ERROR_UNSUPPORTED_OPERATION,
                format!("{provider} does not watch pull requests"),
            )
        })?;
    route.provider.watch_pulls(request)
}

/// The layer threads are placed on: its source in the review, the head the review shows, and
/// the files it changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerTarget {
    /// Review source id.
    pub source_id: String,
    /// Head commit the review shows.
    pub head_oid: String,
    /// Paths the layer changes in the review.
    pub paths: BTreeSet<String>,
}

/// How a thread landed on the review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// On the line the forge places it on, in the same diff the review shows.
    Exact,
    /// Carried from the commit it was written on to where that code is now.
    Moved,
    /// Its code is gone (or could not be followed): on its file.
    OnFile,
    /// The layer no longer changes its file: on the layer.
    OnLayer,
}

impl From<Placement> for bcode_code_review_models::ThreadPlacement {
    fn from(placement: Placement) -> Self {
        match placement {
            Placement::Exact => Self::Exact,
            Placement::Moved => Self::Moved,
            Placement::OnFile => Self::OnFile,
            Placement::OnLayer => Self::OnLayer,
        }
    }
}

/// A thread with its place in the review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacedThread {
    /// The thread.
    pub thread: RemoteThread,
    /// Where the review shows it.
    pub anchor: DraftAnchor,
    /// How it got there.
    pub placement: Placement,
}

/// A line to carry from one commit to another.
struct Carry {
    thread: usize,
    from: String,
    path: String,
    start: u32,
    end: u32,
}

/// Place `activity`'s threads on `target` (in `root`'s repository).
///
/// Lines on the new side are carried through `git diff` when the forge placed the thread on
/// another commit than the review shows; lines on the old side are only placed when the diffs
/// are the same.
#[must_use]
pub fn place_threads(
    root: &Path,
    activity: &PullActivity,
    target: &LayerTarget,
) -> Vec<PlacedThread> {
    let same_head = !activity.head_oid.is_empty() && activity.head_oid == target.head_oid;
    let mut placed: Vec<PlacedThread> = Vec::with_capacity(activity.threads.len());
    let mut carries = Vec::new();
    for thread in &activity.threads {
        let anchor = &thread.anchor;
        let base = PlacedThread {
            thread: thread.clone(),
            anchor: file_anchor(&anchor.path, target),
            placement: Placement::OnFile,
        };
        if !target.paths.contains(&anchor.path) {
            placed.push(PlacedThread {
                anchor: layer_anchor(target),
                placement: Placement::OnLayer,
                ..base
            });
            continue;
        }
        if anchor.file_level {
            placed.push(base);
            continue;
        }
        let current = anchor
            .line
            .map(|end| (anchor.start_line.unwrap_or(end), end));
        let original = anchor
            .original_line
            .map(|end| (anchor.original_start_line.unwrap_or(end), end));
        match (anchor.side, current) {
            (side, Some((start, end))) if same_head => {
                placed.push(PlacedThread {
                    anchor: line_anchor(&anchor.path, target, side, start, end),
                    placement: Placement::Exact,
                    ..base
                });
            }
            (DiffSide::New, _) => {
                // Where the forge has it now, else where it was written.
                let from = current
                    .filter(|_| !activity.head_oid.is_empty())
                    .map(|lines| (activity.head_oid.clone(), lines))
                    .or_else(|| Some((anchor.original_commit_oid.clone()?, original?)));
                if let Some((from, (start, end))) = from {
                    carries.push(Carry {
                        thread: placed.len(),
                        from,
                        path: anchor.path.clone(),
                        start,
                        end,
                    });
                }
                placed.push(base);
            }
            (DiffSide::Old, _) => placed.push(base),
        }
    }
    carry_lines(root, target, &carries, &mut placed);
    placed
}

/// Carry each of `carries` to `target`'s head, placing the threads whose lines survive.
fn carry_lines(root: &Path, target: &LayerTarget, carries: &[Carry], placed: &mut [PlacedThread]) {
    let mut by_commit: BTreeMap<&str, Vec<&Carry>> = BTreeMap::new();
    for carry in carries {
        by_commit
            .entry(carry.from.as_str())
            .or_default()
            .push(carry);
    }
    for (from, carries) in by_commit {
        let paths: BTreeSet<&str> = carries.iter().map(|carry| carry.path.as_str()).collect();
        let Ok(changes) = line_changes(root, from, &target.head_oid, &paths) else {
            continue;
        };
        for carry in carries {
            let hunks = changes.get(&carry.path).map_or(&[][..], Vec::as_slice);
            let (Some(start), Some(end)) =
                (carry_line(hunks, carry.start), carry_line(hunks, carry.end))
            else {
                continue;
            };
            let thread = &mut placed[carry.thread];
            thread.anchor = line_anchor(&carry.path, target, DiffSide::New, start, end);
            thread.placement = Placement::Moved;
        }
    }
}

/// One changed region between two versions of a file: `old_count` lines at `old_start` became
/// `new_count` lines at `new_start`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LineHunk {
    old_start: u32,
    old_count: u32,
    new_start: u32,
    new_count: u32,
}

/// `line` of the old version in the new one: shifted past the regions before it, `None` when a
/// region changed it.
fn carry_line(hunks: &[LineHunk], line: u32) -> Option<u32> {
    let mut shift: i64 = 0;
    for hunk in hunks {
        // A pure insertion's old start is the line before it.
        let first = if hunk.old_count == 0 {
            hunk.old_start + 1
        } else {
            hunk.old_start
        };
        if line < first {
            break;
        }
        if hunk.old_count > 0 && line < hunk.old_start + hunk.old_count {
            return None;
        }
        shift += i64::from(hunk.new_count) - i64::from(hunk.old_count);
    }
    u32::try_from(i64::from(line) + shift)
        .ok()
        .filter(|line| *line > 0)
}

/// The changed regions of `paths` from `from` to `to`, per path (renames followed).
fn line_changes(
    root: &Path,
    from: &str,
    to: &str,
    paths: &BTreeSet<&str>,
) -> Result<BTreeMap<String, Vec<LineHunk>>, DataError> {
    git::validate_oid(from)?;
    git::validate_oid(to)?;
    let mut args = vec![
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "-U0",
        "-M",
        from,
        to,
        "--",
    ];
    for path in paths {
        git::validate_relative_path(path)?;
    }
    args.extend(paths.iter().copied());
    let output = git::output(root, &args)?;
    Ok(parse_line_changes(&String::from_utf8_lossy(&output)))
}

/// The hunk headers of a `-U0` diff, per new path.
fn parse_line_changes(diff: &str) -> BTreeMap<String, Vec<LineHunk>> {
    let mut changes: BTreeMap<String, Vec<LineHunk>> = BTreeMap::new();
    let mut path: Option<String> = None;
    for line in diff.lines() {
        if let Some(name) = line.strip_prefix("+++ ") {
            path = name.strip_prefix("b/").map(str::to_owned);
            if let Some(path) = &path {
                changes.entry(path.clone()).or_default();
            }
        } else if let (Some(header), Some(path)) = (line.strip_prefix("@@ -"), &path)
            && let Some(hunk) = parse_hunk_header(header)
        {
            changes.entry(path.clone()).or_default().push(hunk);
        }
    }
    changes
}

/// `a[,b] +c[,d] @@…` (the part after `@@ -`).
fn parse_hunk_header(header: &str) -> Option<LineHunk> {
    let (ranges, _) = header.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    let range = |text: &str| -> Option<(u32, u32)> {
        match text.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((text.parse().ok()?, 1)),
        }
    };
    let (old_start, old_count) = range(old)?;
    let (new_start, new_count) = range(new)?;
    Some(LineHunk {
        old_start,
        old_count,
        new_start,
        new_count,
    })
}

fn empty_anchor(target: &LayerTarget) -> DraftAnchor {
    DraftAnchor {
        kind: ReviewAnchorKind::Review,
        file_path: crate::document::REVIEW_LEVEL_PATH.to_owned(),
        diff_row: 0,
        start_diff_row: None,
        end_diff_row: None,
        old_start: None,
        old_end: None,
        new_start: None,
        new_end: None,
        old_line: None,
        new_line: None,
        line_kind: ReviewLineKind::Context,
        is_file_anchor: false,
        surface_id: None,
        source_id: Some(target.source_id.clone()),
        head_oid: Some(target.head_oid.clone()),
        blob_oid: None,
        reply_to: None,
    }
}

/// The layer itself (its overview).
fn layer_anchor(target: &LayerTarget) -> DraftAnchor {
    empty_anchor(target)
}

/// The file `path` of the layer.
fn file_anchor(path: &str, target: &LayerTarget) -> DraftAnchor {
    DraftAnchor {
        kind: ReviewAnchorKind::File,
        file_path: path.to_owned(),
        ..empty_anchor(target)
    }
}

/// Lines `start..=end` of `path` on `side`.
fn line_anchor(
    path: &str,
    target: &LayerTarget,
    side: DiffSide,
    start: u32,
    end: u32,
) -> DraftAnchor {
    let (start, end) = (start.min(end), start.max(end));
    let mut anchor = DraftAnchor {
        kind: ReviewAnchorKind::Range,
        file_path: path.to_owned(),
        diff_row: u64::from(end),
        ..empty_anchor(target)
    };
    match side {
        DiffSide::New => {
            anchor.new_start = Some(start);
            anchor.new_end = Some(end);
            anchor.new_line = Some(end);
            anchor.line_kind = ReviewLineKind::Added;
        }
        DiffSide::Old => {
            anchor.old_start = Some(start);
            anchor.old_end = Some(end);
            anchor.old_line = Some(end);
            anchor.line_kind = ReviewLineKind::Removed;
        }
    }
    anchor
}

/// Whether a review given on `commit` still applies to a layer's current version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Currency {
    /// Given on the current head.
    Current,
    /// Given on an earlier version, and the layer was only rebased (or retargeted) since.
    Rebased,
    /// The layer's change differs since.
    Outdated,
    /// The history does not say.
    Unknown,
}

/// Whether a review given on `commit` applies to the newest version of `layer`.
#[must_use]
pub fn review_currency(layer: &LayerTimeline, commit: &str, current_head: &str) -> Currency {
    if commit == current_head {
        return Currency::Current;
    }
    let Some(given) = layer
        .versions
        .iter()
        .position(|version| version.version.head_oid == commit)
    else {
        return Currency::Unknown;
    };
    let since = &layer.versions[given + 1..];
    if since.is_empty()
        || since
            .iter()
            .any(|version| version.kind == ChangeKind::Unavailable)
    {
        return Currency::Unknown;
    }
    if since.iter().all(|version| {
        matches!(
            version.kind,
            ChangeKind::RebaseOnly | ChangeKind::Retargeted
        )
    }) {
        Currency::Rebased
    } else {
        Currency::Outdated
    }
}

/// Threads that wait on the viewer: open threads where someone else spoke last, on a pull
/// request the viewer wrote or in a thread the viewer took part in.
#[must_use]
pub fn waiting_on_viewer(activity: &PullActivity, thread: &RemoteThread) -> bool {
    if thread.resolved {
        return false;
    }
    let Some(last) = thread.comments.last() else {
        return false;
    };
    if last.author.is_viewer {
        return false;
    }
    activity.viewer.is_author
        || thread
            .comments
            .iter()
            .any(|comment| comment.author.is_viewer)
}

/// The repository root of `repo_path`, for placement.
///
/// # Errors
/// When `repo_path` is not in a repository.
pub fn repository_root(repo_path: &Path) -> Result<PathBuf, DataError> {
    let output = git::output(repo_path, &["rev-parse", "--show-toplevel"])?;
    Ok(PathBuf::from(String::from_utf8_lossy(&output).trim()))
}

#[cfg(test)]
mod tests;
