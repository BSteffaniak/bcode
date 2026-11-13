//! Stack history: the versions each pull request of a stack went through, and how each version
//! changed from the one before. See `docs/review-stack-history.md`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;

/// Store operation that records layer versions (merging what each source saw).
pub const OP_REVIEW_HISTORY_RECORD: &str = "review.history.record";
/// Store operation that lists the recorded versions of some pull requests.
pub const OP_REVIEW_HISTORY_LIST: &str = "review.history.list";
/// Provider operation that reports pull requests' push history (see
/// [`ReviewSourcePullsHistoryRequest`]).
pub const OP_REVIEW_SOURCE_PULLS_HISTORY: &str = "review_source.pulls.history";

/// What saw a layer version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionSource {
    /// The forge's timeline (a push it recorded).
    Forge,
    /// A review resolved or confirmed the stack while it was this.
    Observed,
    /// A local or remote-tracking branch's reflog.
    Reflog,
}

/// One state of one pull request: its head, and what it was based on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerVersion {
    /// Provider id.
    pub provider: String,
    /// Provider repository.
    pub repository: String,
    /// Pull request number.
    pub number: u64,
    /// Head commit.
    pub head_oid: String,
    /// The base branch's commit at the time, when known.
    #[serde(default)]
    pub base_oid: Option<String>,
    /// The base branch at the time, when known.
    #[serde(default)]
    pub base_branch: Option<String>,
    /// When it was pushed (or, when that is unknown, first seen), in milliseconds since the Unix
    /// epoch.
    pub seen_at_ms: u64,
    /// Who pushed it, when known.
    #[serde(default)]
    pub actor: Option<String>,
    /// What saw it.
    #[serde(default)]
    pub sources: BTreeSet<VersionSource>,
}

/// Request payload for [`OP_REVIEW_HISTORY_RECORD`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordLayerVersionsRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// Versions to record: new ones are added; known ones (same pull request and head) gain the
    /// new sources and whatever the new record knows that the old did not, and keep the earlier
    /// time.
    pub versions: Vec<LayerVersion>,
}

/// Response payload for [`OP_REVIEW_HISTORY_RECORD`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordLayerVersionsResponse {
    /// Versions not known before.
    pub added: usize,
}

/// Request payload for [`OP_REVIEW_HISTORY_LIST`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListLayerVersionsRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// Provider id.
    pub provider: String,
    /// Provider repository.
    pub repository: String,
    /// Pull requests whose versions to list.
    pub numbers: Vec<u64>,
}

/// Response payload for [`OP_REVIEW_HISTORY_LIST`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListLayerVersionsResponse {
    /// Their versions, oldest first per pull request.
    pub versions: Vec<LayerVersion>,
}

/// How a layer version changed from the layer's version before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// The first version known.
    First,
    /// The layer's own change differs.
    Content,
    /// Only what the layer is based on moved: its change, replayed on the new base, is exactly
    /// the new version.
    RebaseOnly,
    /// Commits were added on top of the version before (a plain push).
    CommitsAdded,
    /// Its base branch changed.
    Retargeted,
    /// Its commits (or the version before's) are not in the local repository.
    Unavailable,
}

impl ChangeKind {
    /// Whether the version changes what the layer does (as opposed to only where it sits).
    #[must_use]
    pub const fn changes_content(self) -> bool {
        matches!(
            self,
            Self::Content | Self::CommitsAdded | Self::Retargeted | Self::First
        )
    }
}

/// A forge's record of one push to a pull request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PullHistoryEvent {
    /// The head was replaced (a force push, as a rebase or restack makes).
    ForcePushed {
        /// When, in milliseconds since the Unix epoch.
        at_ms: u64,
        /// Who pushed.
        #[serde(default)]
        actor: Option<String>,
        /// The head before, when the forge still has it.
        #[serde(default)]
        before_oid: Option<String>,
        /// The head after, when the forge still has it.
        #[serde(default)]
        after_oid: Option<String>,
    },
    /// Commits were pushed on top; this is the newest of them.
    CommitsPushed {
        /// When (the commit's time), in milliseconds since the Unix epoch.
        at_ms: u64,
        /// The head after.
        head_oid: String,
    },
    /// The base branch changed.
    BaseChanged {
        /// When, in milliseconds since the Unix epoch.
        at_ms: u64,
        /// The base branch before.
        #[serde(default)]
        from: Option<String>,
        /// The base branch after.
        #[serde(default)]
        to: Option<String>,
    },
}

/// One pull request's push history, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullHistory {
    /// Pull request number.
    pub number: u64,
    /// Its pushes.
    pub events: Vec<PullHistoryEvent>,
}

/// Request payload for [`OP_REVIEW_SOURCE_PULLS_HISTORY`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSourcePullsHistoryRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// Provider repository identifier; `None` detects it from the repository's remotes.
    #[serde(default)]
    pub repository: Option<String>,
    /// Pull requests whose history to report.
    pub numbers: Vec<u64>,
    /// Also make the commits the history names local where the forge still has them, so the
    /// versions can be compared.
    #[serde(default)]
    pub fetch: bool,
}

/// Response payload for [`OP_REVIEW_SOURCE_PULLS_HISTORY`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSourcePullsHistoryResponse {
    /// Provider repository identifier.
    pub repository: String,
    /// One history per requested pull request the forge knows.
    pub histories: Vec<PullHistory>,
}

/// One version of a layer on its timeline: how it changed, and what it was based on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineVersion {
    /// The version.
    pub version: LayerVersion,
    /// How it changed from the one before.
    pub kind: ChangeKind,
    /// The commit its own change starts from (where it forked from what it is based on), when
    /// known: comparisons start here.
    #[serde(default)]
    pub fork_oid: Option<String>,
    /// Whether this reviewer reviewed this version.
    #[serde(default)]
    pub reviewed: bool,
}

/// One layer's versions, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerTimeline {
    /// Pull request number.
    pub number: u64,
    /// Title, as the stack names it now.
    pub title: String,
    /// Head branch now.
    pub head_branch: String,
    /// Its versions.
    pub versions: Vec<TimelineVersion>,
}

/// Versions of several layers pushed together (within the push window).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackPush {
    /// When the push started, in milliseconds since the Unix epoch.
    pub at_ms: u64,
    /// Each layer it changed, with how.
    pub layers: Vec<(u64, ChangeKind)>,
}

impl StackPush {
    /// The layers whose content it changed, and how many it only rebased.
    #[must_use]
    pub fn split(&self) -> (Vec<u64>, usize) {
        let changed: Vec<u64> = self
            .layers
            .iter()
            .filter(|(_, kind)| kind.changes_content())
            .map(|(number, _)| *number)
            .collect();
        let rebased = self
            .layers
            .iter()
            .filter(|(_, kind)| *kind == ChangeKind::RebaseOnly)
            .count();
        (changed, rebased)
    }
}

/// A stack's history: each layer's timeline and the pushes across them, oldest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackTimeline {
    /// Layers, bottom first.
    pub layers: Vec<LayerTimeline>,
    /// Pushes, oldest first.
    pub pushes: Vec<StackPush>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_push_says_which_layers_changed_and_how_many_only_moved() {
        let push = StackPush {
            at_ms: 0,
            layers: vec![
                (1, ChangeKind::Content),
                (2, ChangeKind::RebaseOnly),
                (3, ChangeKind::RebaseOnly),
                (4, ChangeKind::CommitsAdded),
                (5, ChangeKind::Unavailable),
            ],
        };
        assert_eq!(push.split(), (vec![1, 4], 2));
        let event: PullHistoryEvent =
            serde_json::from_str(r#"{"kind":"force_pushed","at_ms":5,"after_oid":"abc"}"#)
                .expect("event");
        assert_eq!(
            event,
            PullHistoryEvent::ForcePushed {
                at_ms: 5,
                actor: None,
                before_oid: None,
                after_oid: Some("abc".into()),
            }
        );
    }
}
