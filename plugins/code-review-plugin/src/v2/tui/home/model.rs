//! Home model (spec 7.2): review groups, filtering, table rows, and suggestions derived from
//! repository state. Pure functions over service answers, so every rule is unit-tested.

use super::super::entry::Entry;
use crate::v2::provider::ProviderRoute;
use bcode_code_review_models::{
    PullRequestSummary, ReviewRepoStateResponse, ReviewSourceKind, ReviewStack, ReviewWorkspace,
    ReviewWorkspaceListItem,
};
use std::collections::{BTreeMap, BTreeSet};

/// Group of a review in the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Group {
    /// No (included) sources, drafts changed after the last publish, or new commits since the
    /// last pass.
    NeedsAttention,
    /// Everything else that is not published or archived (including unpublished drafts).
    InProgress,
    /// Published, with no draft changes since.
    Published,
    /// Archived (collapsed by default).
    Archived,
}

impl Group {
    /// Every group, in table order.
    pub const ALL: [Self; 4] = [
        Self::NeedsAttention,
        Self::InProgress,
        Self::Published,
        Self::Archived,
    ];

    /// Heading text.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::NeedsAttention => "Needs attention",
            Self::InProgress => "In progress",
            Self::Published => "Published",
            Self::Archived => "Archived",
        }
    }
}

/// What the service reported about one review's current sources.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceStats {
    /// Changed files across the sources that resolved.
    pub files: u64,
    /// Added lines.
    pub additions: u64,
    /// Removed lines.
    pub deletions: u64,
    /// Sources whose head commit moved since the last recorded pass.
    pub new_commits: bool,
    /// Sources that did not resolve without asking a provider (pull requests and stacks are
    /// counted only after the review is opened).
    pub unresolved: usize,
}

/// Whether drafts changed after the last publish.
#[must_use]
pub fn changed_since_publish(item: &ReviewWorkspaceListItem) -> bool {
    item.draft_count > 0
        && item.last_publish.as_ref().is_some_and(|publish| {
            item.workspace
                .updated_at_ms
                .is_some_and(|updated| updated > publish.created_at_ms)
        })
}

/// The group of `item`, given its stats when known.
#[must_use]
pub fn group_of(item: &ReviewWorkspaceListItem, stats: Option<&WorkspaceStats>) -> Group {
    let workspace = &item.workspace;
    if workspace.archived_at_ms.is_some() {
        return Group::Archived;
    }
    let no_sources = workspace.sources.iter().all(|source| !source.included);
    if no_sources || changed_since_publish(item) || stats.is_some_and(|stats| stats.new_commits) {
        return Group::NeedsAttention;
    }
    if item.last_publish.is_some() {
        Group::Published
    } else {
        Group::InProgress
    }
}

/// Lowercase words of a filter query.
#[must_use]
pub fn filter_words(query: &str) -> Vec<String> {
    query.split_whitespace().map(str::to_lowercase).collect()
}

/// Whether every filter word occurs in the review's title, source labels, or id.
#[must_use]
pub fn matches_filter(workspace: &ReviewWorkspace, words: &[String]) -> bool {
    if words.is_empty() {
        return true;
    }
    let haystack = std::iter::once(workspace.title.as_str())
        .chain(workspace.sources.iter().map(|source| source.label.as_str()))
        .chain(std::iter::once(workspace.id.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase();
    words.iter().all(|word| haystack.contains(word.as_str()))
}

/// One row of the reviews table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowRef {
    /// A group heading with its review count.
    Group {
        /// The group.
        group: Group,
        /// Reviews in the group (matching the filter).
        count: usize,
        /// Whether its reviews are hidden.
        collapsed: bool,
    },
    /// A review: index into the listed items.
    Review(usize),
}

/// Stable identity of a table row, kept across reloads.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum RowKey {
    /// A group heading.
    Group(Group),
    /// A review, by workspace id.
    Review(String),
}

/// The table rows: groups in order with their matching reviews, most recently updated first.
///
/// Empty groups are left out. A collapsed group shows only its heading, except while a filter is
/// active (matches are never hidden).
#[must_use]
pub fn build_rows(
    items: &[ReviewWorkspaceListItem],
    stats: &BTreeMap<String, WorkspaceStats>,
    words: &[String],
    collapsed: &BTreeSet<Group>,
) -> Vec<RowRef> {
    let mut grouped: BTreeMap<Group, Vec<usize>> = BTreeMap::new();
    for (index, item) in items.iter().enumerate() {
        if !matches_filter(&item.workspace, words) {
            continue;
        }
        let group = group_of(item, stats.get(&item.workspace.id));
        grouped.entry(group).or_default().push(index);
    }
    let mut rows = Vec::new();
    for group in Group::ALL {
        let Some(mut members) = grouped.remove(&group) else {
            continue;
        };
        members.sort_by_key(|index| {
            std::cmp::Reverse(items[*index].workspace.updated_at_ms.unwrap_or(0))
        });
        let hidden = collapsed.contains(&group) && words.is_empty();
        rows.push(RowRef::Group {
            group,
            count: members.len(),
            collapsed: hidden,
        });
        if !hidden {
            rows.extend(members.into_iter().map(RowRef::Review));
        }
    }
    rows
}

/// Compact relative time of `timestamp_ms` at `now_ms`.
#[must_use]
pub fn relative_time(now_ms: u64, timestamp_ms: u64) -> String {
    let seconds = now_ms.saturating_sub(timestamp_ms) / 1_000;
    match seconds {
        0..=59 => "now".to_owned(),
        60..=3_599 => format!("{}m ago", seconds / 60),
        3_600..=86_399 => format!("{}h ago", seconds / 3_600),
        86_400..=604_799 => format!("{}d ago", seconds / 86_400),
        _ => format!("{}w ago", seconds / 604_800),
    }
}

/// What starting a suggestion does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuggestionAction {
    /// Open a review of an entry.
    Start(Entry),
    /// Continue an existing review.
    Open(Box<ReviewWorkspace>),
    /// Open the new-review wizard.
    New,
}

/// Kind of a suggestion (its icon and color).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestionKind {
    /// Uncommitted working-tree or index changes.
    Uncommitted,
    /// The current branch ahead of its base.
    Branch,
    /// A pull request requesting my review.
    ReviewRequested,
    /// A stack (or the pull request) containing the current branch.
    Stack,
    /// A review with new commits since the last pass.
    NewCommits,
    /// The new-review wizard.
    New,
}

/// One suggestion on home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    /// Stable id.
    pub id: String,
    /// Kind.
    pub kind: SuggestionKind,
    /// Title.
    pub title: String,
    /// Muted detail.
    pub detail: String,
    /// What starting it does.
    pub action: SuggestionAction,
}

/// Provider answers about pull requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullsAnswer {
    /// The provider asked.
    pub route: ProviderRoute,
    /// Repository identifier.
    pub repository: String,
    /// Open pull requests, review requested from me first.
    pub pulls: Vec<PullRequestSummary>,
}

/// Inputs of [`suggestions`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SuggestionInputs<'a> {
    /// Repository state.
    pub state: Option<&'a ReviewRepoStateResponse>,
    /// Pull requests.
    pub pulls: Option<&'a PullsAnswer>,
    /// The stack containing the current branch's pull request.
    pub stack: Option<&'a ReviewStack>,
    /// Listed reviews.
    pub items: &'a [ReviewWorkspaceListItem],
    /// Their stats.
    pub stats: Option<&'a BTreeMap<String, WorkspaceStats>>,
}

/// Most pull requests requesting my review suggested at once.
const MAX_REQUESTED: usize = 3;
/// Most reviews with new commits suggested at once.
const MAX_NEW_COMMITS: usize = 2;

fn plural(count: u64, one: &str, many: &str) -> String {
    if count == 1 {
        format!("1 {one}")
    } else {
        format!("{count} {many}")
    }
}

/// Suggestions derived from repository state.
///
/// In order: uncommitted changes, the current branch ahead of its base, the stack (or pull
/// request) of the current branch, pull requests requesting my review, reviews with new commits
/// since the last pass, and always the wizard last.
#[must_use]
pub fn suggestions(inputs: &SuggestionInputs<'_>, minus: &str) -> Vec<Suggestion> {
    let mut out = Vec::new();
    if let Some(state) = inputs.state {
        uncommitted(state, minus, &mut out);
        if let (Some(branch), Some(base), Some(ahead)) =
            (state.branch.as_ref(), state.base.as_ref(), state.ahead)
            && ahead > 0
        {
            out.push(Suggestion {
                id: "branch".to_owned(),
                kind: SuggestionKind::Branch,
                title: format!("Branch {branch}"),
                detail: format!(
                    "{} ahead of {base}",
                    plural(u64::from(ahead), "commit", "commits")
                ),
                action: SuggestionAction::Start(Entry::Sources {
                    title: Some(branch.clone()),
                    sources: vec![ReviewSourceKind::CurrentBranch { base: None }],
                }),
            });
        }
    }
    let mut suggested = BTreeSet::new();
    if let Some(pulls) = inputs.pulls {
        let provider = pulls.route.manifest.id.clone();
        let branch = inputs.state.and_then(|state| state.branch.as_deref());
        if let Some(own) = branch_pull(pulls, branch, inputs.stack) {
            // The branch's own pull request is not suggested twice.
            suggested.extend(
                pulls
                    .pulls
                    .iter()
                    .filter(|pull| Some(pull.head_branch.as_str()) == branch)
                    .map(|pull| pull.number),
            );
            out.push(own);
        }
        for pull in pulls
            .pulls
            .iter()
            .filter(|pull| pull.review_requested_from_me && !suggested.contains(&pull.number))
            .take(MAX_REQUESTED)
        {
            let author = pull
                .author
                .as_deref()
                .map_or_else(String::new, |author| format!(" by {author}"));
            out.push(Suggestion {
                id: format!("pull-{}", pull.number),
                kind: SuggestionKind::ReviewRequested,
                title: format!("Review requested: #{}", pull.number),
                detail: format!("{}{author}", pull.title),
                action: SuggestionAction::Start(Entry::Sources {
                    title: Some(format!("#{} {}", pull.number, pull.title)),
                    sources: vec![ReviewSourceKind::PullRequest {
                        provider: provider.clone(),
                        repository: pulls.repository.clone(),
                        number: pull.number,
                    }],
                }),
            });
        }
    }
    if let Some(stats) = inputs.stats {
        out.extend(moved_reviews(inputs.items, stats));
    }
    out.push(Suggestion {
        id: "new".to_owned(),
        kind: SuggestionKind::New,
        title: "New review…".to_owned(),
        detail: "branches, commits, pull requests, stacks, files".to_owned(),
        action: SuggestionAction::New,
    });
    out
}

/// The stack (or the pull request) of the current branch's pull request.
fn branch_pull(
    pulls: &PullsAnswer,
    branch: Option<&str>,
    stack: Option<&ReviewStack>,
) -> Option<Suggestion> {
    let own = pulls
        .pulls
        .iter()
        .find(|pull| Some(pull.head_branch.as_str()) == branch)?;
    let provider = pulls.route.manifest.id.clone();
    let layers = stack.map_or(1, |stack| stack.layers.len());
    let (title, kind) = if layers > 1 {
        let top = stack
            .and_then(|stack| stack.layers.last())
            .map_or(own.number, |layer| layer.pull.number);
        (
            format!("Stack of #{} ({layers} layers)", own.number),
            ReviewSourceKind::Stack {
                provider,
                repository: pulls.repository.clone(),
                top,
            },
        )
    } else {
        (
            format!("Your pull request #{}", own.number),
            ReviewSourceKind::PullRequest {
                provider,
                repository: pulls.repository.clone(),
                number: own.number,
            },
        )
    };
    Some(Suggestion {
        id: format!("stack-{}", own.number),
        kind: SuggestionKind::Stack,
        detail: own.title.clone(),
        action: SuggestionAction::Start(Entry::Sources {
            title: Some(format!("#{} {}", own.number, own.title)),
            sources: vec![kind],
        }),
        title,
    })
}

/// Reviews whose commit heads moved since the recorded pass, most recently updated first.
fn moved_reviews(
    items: &[ReviewWorkspaceListItem],
    stats: &BTreeMap<String, WorkspaceStats>,
) -> Vec<Suggestion> {
    let mut moved: Vec<&ReviewWorkspaceListItem> = items
        .iter()
        .filter(|item| {
            item.workspace.archived_at_ms.is_none()
                && stats
                    .get(&item.workspace.id)
                    .is_some_and(|stats| stats.new_commits)
        })
        .collect();
    moved.sort_by_key(|item| std::cmp::Reverse(item.workspace.updated_at_ms.unwrap_or(0)));
    moved
        .into_iter()
        .take(MAX_NEW_COMMITS)
        .map(|item| Suggestion {
            id: format!("moved-{}", item.workspace.id),
            kind: SuggestionKind::NewCommits,
            title: format!("New commits: {}", item.workspace.title),
            detail: "since your last pass".to_owned(),
            action: SuggestionAction::Open(Box::new(item.workspace.clone())),
        })
        .collect()
}

fn uncommitted(state: &ReviewRepoStateResponse, minus: &str, out: &mut Vec<Suggestion>) {
    let (unstaged, staged) = (state.unstaged, state.staged);
    let (kind, title) = match (unstaged.is_empty(), staged.is_empty()) {
        (true, true) => return,
        (false, true) => (ReviewSourceKind::WorkingTreeUnstaged, "Unstaged changes"),
        (true, false) => (ReviewSourceKind::IndexStaged, "Staged changes"),
        (false, false) => (ReviewSourceKind::WorkingTreeAndIndex, "Uncommitted changes"),
    };
    let mut parts = Vec::new();
    if !unstaged.is_empty() {
        parts.push(format!(
            "{} unstaged",
            plural(u64::from(unstaged.files), "file", "files")
        ));
    }
    if !staged.is_empty() {
        parts.push(format!(
            "{} staged",
            plural(u64::from(staged.files), "file", "files")
        ));
    }
    let additions = unstaged.additions + staged.additions;
    let deletions = unstaged.deletions + staged.deletions;
    out.push(Suggestion {
        id: "uncommitted".to_owned(),
        kind: SuggestionKind::Uncommitted,
        title: title.to_owned(),
        detail: format!("{} · +{additions} {minus}{deletions}", parts.join(", ")),
        action: SuggestionAction::Start(Entry::Sources {
            title: Some(title.to_owned()),
            sources: vec![kind],
        }),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_code_review_models::{
        ReviewChangeCount, ReviewPublishRecord, ReviewSourceCapabilities,
        ReviewSourceProviderManifest, ReviewStackLayer,
    };
    use std::path::PathBuf;

    fn item(id: &str, updated: u64) -> ReviewWorkspaceListItem {
        let mut workspace = super::super::super::entry::transient_workspace(
            PathBuf::from("/repo"),
            Some(format!("Review {id}")),
            vec![ReviewSourceKind::LastCommit],
        );
        workspace.id = id.to_owned();
        workspace.updated_at_ms = Some(updated);
        ReviewWorkspaceListItem {
            workspace,
            thread_count: 0,
            draft_count: 0,
            last_publish: None,
        }
    }

    fn publish(at: u64) -> ReviewPublishRecord {
        ReviewPublishRecord {
            id: "p".into(),
            workspace_id: None,
            review_id: "r".into(),
            publisher_id: "github".into(),
            submitted: true,
            published_thread_anchors: Vec::new(),
            output: None,
            message: "ok".into(),
            created_at_ms: at,
        }
    }

    #[test]
    fn groups_follow_sources_publish_state_drafts_and_new_commits() {
        let active = item("a", 10);
        assert_eq!(group_of(&active, None), Group::InProgress);
        let mut archived = item("b", 10);
        archived.workspace.archived_at_ms = Some(11);
        assert_eq!(group_of(&archived, None), Group::Archived);
        let mut empty = item("c", 10);
        empty.workspace.sources.clear();
        assert_eq!(group_of(&empty, None), Group::NeedsAttention);
        let mut excluded = item("d", 10);
        excluded.workspace.sources[0].included = false;
        assert_eq!(group_of(&excluded, None), Group::NeedsAttention);
        let mut published = item("e", 10);
        published.last_publish = Some(publish(20));
        published.draft_count = 2;
        assert_eq!(group_of(&published, None), Group::Published);
        published.workspace.updated_at_ms = Some(30);
        assert_eq!(group_of(&published, None), Group::NeedsAttention);
        let moved = WorkspaceStats {
            new_commits: true,
            ..WorkspaceStats::default()
        };
        assert_eq!(group_of(&active, Some(&moved)), Group::NeedsAttention);
        let mut drafts = item("f", 10);
        drafts.draft_count = 3;
        assert_eq!(group_of(&drafts, None), Group::InProgress);
    }

    #[test]
    fn rows_group_sort_filter_and_collapse() {
        let mut items = vec![item("old", 1), item("new", 5), item("gone", 3)];
        items[2].workspace.archived_at_ms = Some(4);
        items[0].workspace.sources.clear();
        let stats = BTreeMap::new();
        let collapsed = BTreeSet::from([Group::Archived]);
        let rows = build_rows(&items, &stats, &[], &collapsed);
        assert_eq!(
            rows,
            vec![
                RowRef::Group {
                    group: Group::NeedsAttention,
                    count: 1,
                    collapsed: false
                },
                RowRef::Review(0),
                RowRef::Group {
                    group: Group::InProgress,
                    count: 1,
                    collapsed: false
                },
                RowRef::Review(1),
                RowRef::Group {
                    group: Group::Archived,
                    count: 1,
                    collapsed: true
                },
            ]
        );
        // A filter shows archived matches and drops groups without matches.
        let rows = build_rows(&items, &stats, &filter_words("GONE"), &collapsed);
        assert_eq!(
            rows,
            vec![
                RowRef::Group {
                    group: Group::Archived,
                    count: 1,
                    collapsed: false
                },
                RowRef::Review(2),
            ]
        );
        // Every word must match (title, source label, or id).
        assert!(matches_filter(
            &items[1].workspace,
            &filter_words("review last")
        ));
        assert!(!matches_filter(
            &items[1].workspace,
            &filter_words("review staged")
        ));
    }

    #[test]
    fn relative_times_are_compact() {
        let minute = 60_000;
        assert_eq!(relative_time(10 * minute, 10 * minute), "now");
        assert_eq!(relative_time(10 * minute, 5 * minute), "5m ago");
        assert_eq!(relative_time(300 * minute, 0), "5h ago");
        assert_eq!(relative_time(3 * 1_440 * minute, 0), "3d ago");
        assert_eq!(relative_time(21 * 1_440 * minute, 0), "3w ago");
        assert_eq!(relative_time(0, minute), "now");
    }

    fn pull(number: u64, head: &str, requested: bool) -> PullRequestSummary {
        PullRequestSummary {
            number,
            title: format!("Change {number}"),
            author: Some("octo".into()),
            base_branch: "main".into(),
            head_branch: head.into(),
            head_oid: String::new(),
            base_oid: None,
            review_requested_from_me: requested,
            draft: false,
            updated: None,
            url: None,
        }
    }

    #[test]
    fn suggestions_cover_repository_state_providers_and_moved_reviews() {
        let state = ReviewRepoStateResponse {
            branch: Some("feature".into()),
            head_oid: Some("h".into()),
            base: Some("origin/main".into()),
            merge_base_oid: Some("m".into()),
            ahead: Some(3),
            unstaged: ReviewChangeCount {
                files: 2,
                additions: 5,
                deletions: 1,
            },
            staged: ReviewChangeCount::default(),
        };
        let route = ProviderRoute {
            plugin_id: "p".into(),
            manifest: ReviewSourceProviderManifest {
                id: "fake".into(),
                label: "Fake".into(),
                capabilities: ReviewSourceCapabilities::default(),
            },
        };
        let pulls = PullsAnswer {
            route,
            repository: "o/r".into(),
            pulls: vec![
                pull(7, "other", true),
                pull(8, "feature", true),
                pull(9, "x", false),
            ],
        };
        let stack = ReviewStack {
            trunk: "main".into(),
            rooted_at_default_branch: true,
            layers: vec![
                ReviewStackLayer {
                    pull: pull(8, "feature", true),
                    parent: None,
                },
                ReviewStackLayer {
                    pull: pull(10, "feature-2", false),
                    parent: Some(8),
                },
            ],
            requested: 8,
            siblings: Vec::new(),
            warnings: Vec::new(),
        };
        let listed = vec![item("w", 1)];
        let moved = BTreeMap::from([(
            "w".to_owned(),
            WorkspaceStats {
                new_commits: true,
                ..WorkspaceStats::default()
            },
        )]);
        let out = suggestions(
            &SuggestionInputs {
                state: Some(&state),
                pulls: Some(&pulls),
                stack: Some(&stack),
                items: &listed,
                stats: Some(&moved),
            },
            "-",
        );
        let titles: Vec<&str> = out.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "Unstaged changes",
                "Branch feature",
                "Stack of #8 (2 layers)",
                "Review requested: #7",
                "New commits: Review w",
                "New review…",
            ]
        );
        assert_eq!(out[0].detail, "2 files unstaged · +5 -1");
        assert_eq!(out[1].detail, "3 commits ahead of origin/main");
        assert_eq!(
            out[2].action,
            SuggestionAction::Start(Entry::Sources {
                title: Some("#8 Change 8".into()),
                sources: vec![ReviewSourceKind::Stack {
                    provider: "fake".into(),
                    repository: "o/r".into(),
                    top: 10,
                }],
            })
        );
        // Nothing to suggest but the wizard on a clean repository without providers.
        let clean = ReviewRepoStateResponse::default();
        let out = suggestions(
            &SuggestionInputs {
                state: Some(&clean),
                ..SuggestionInputs::default()
            },
            "-",
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].action, SuggestionAction::New);
    }
}
