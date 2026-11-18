//! Comparisons across a pull request's versions: looks (what a reviewer saw), layer deltas (what
//! changed in a layer between two versions, rebase taken out), and line marks (which lines of the
//! current diff are new since a baseline). See `docs/review-stack-history.md`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Store operation that records looks (upserting by pull request, head, and kind).
pub const OP_REVIEW_LOOKS_RECORD: &str = "review.looks.record";
/// Store operation that lists the looks of some pull requests.
pub const OP_REVIEW_LOOKS_LIST: &str = "review.looks.list";
/// Data operation that compares layers between versions (see [`ReviewCompareLayersRequest`]).
pub const OP_REVIEW_COMPARE_LAYERS: &str = "review.compare.layers";
/// Data operation that marks one file's lines new since a baseline (see
/// [`ReviewCompareMarksRequest`]).
pub const OP_REVIEW_COMPARE_MARKS: &str = "review.compare.marks";
/// Data operation that keeps (or stops keeping) version heads' commits with refs.
pub const OP_REVIEW_VERSIONS_KEEP: &str = "review.versions.keep";

/// Reference namespace under which version heads are kept, so git does not collect them after a
/// force push drops them from their branch.
pub const VERSION_REF_NAMESPACE: &str = "refs/bcode-review/version";

/// Reference namespace under which the heads of looks are kept (apart from
/// [`VERSION_REF_NAMESPACE`], which fetched versions use, so releasing a pruned look never drops
/// a fetched version's ref).
pub const LOOK_REF_NAMESPACE: &str = "refs/bcode-review/looked";

/// Most distinct heads whose looks are kept per pull request; older heads are pruned.
pub const MAX_LOOK_HEADS_PER_PULL: usize = 32;

/// Most layers one [`ReviewCompareLayersRequest`] compares.
pub const REVIEW_COMPARE_MAX_LAYERS: usize = 256;

/// What a review compares its pull request layers with (kept in the workspace's presentation
/// state, so a reopened review compares the same way; [`Self::LastLooked`] resolves again on
/// every open).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewCompareBaseline {
    /// Each layer's version the reviewer last looked at.
    #[default]
    LastLooked,
    /// Each layer's version before the stack's latest push.
    PreviousPush,
    /// Each layer's version at `from_ms` against its version at `to_ms` (`None`: now).
    Between {
        /// From the versions current then, in milliseconds since the Unix epoch.
        from_ms: u64,
        /// To the versions current then; `None` for the current versions.
        #[serde(default)]
        to_ms: Option<u64>,
    },
    /// No comparison: the full change.
    Off,
}

/// How a review compares its pull request layers, kept in its presentation state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReviewCompareChoice {
    /// What layers are compared with; `None` follows configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<ReviewCompareBaseline>,
    /// Only pull requests with real changes since the baseline are shown.
    #[serde(default)]
    pub only_changed: bool,
    /// The baseline was chosen as a time this long before now (`1 hour ago`, `yesterday`):
    /// a reopened review compares with that long before it reopens, and names the choice so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ago_ms: Option<u64>,
}

impl ReviewCompareChoice {
    /// Whether nothing was chosen.
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// How a reviewer looked at a version of a pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LookKind {
    /// The layer was the current layer, with the screen focused, for the dwell time.
    Opened,
    /// A file of the layer was marked viewed.
    Viewed,
    /// A draft comment or reply was made on the layer.
    Commented,
    /// An AI ask was made on the layer.
    Asked,
    /// A review pass was completed, or the review was published.
    Pass,
    /// The forge reports a review (a verdict) by the reviewer on that commit.
    Reviewed,
}

impl LookKind {
    /// Every kind.
    pub const ALL: [Self; 6] = [
        Self::Opened,
        Self::Viewed,
        Self::Commented,
        Self::Asked,
        Self::Pass,
        Self::Reviewed,
    ];

    /// Whether the look shows engagement with the version (anything but opening it).
    #[must_use]
    pub const fn engaged(self) -> bool {
        !matches!(self, Self::Opened)
    }

    /// Stable name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Opened => "opened",
            Self::Viewed => "viewed",
            Self::Commented => "commented",
            Self::Asked => "asked",
            Self::Pass => "pass",
            Self::Reviewed => "reviewed",
        }
    }

    /// The kind named `name`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == name)
    }
}

/// One look: the reviewer saw this version of this pull request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerLook {
    /// Provider id.
    pub provider: String,
    /// Provider repository.
    pub repository: String,
    /// Pull request number.
    pub number: u64,
    /// The head the review showed.
    pub head_oid: String,
    /// Where that version's own change starts, when known.
    #[serde(default)]
    pub fork_oid: Option<String>,
    /// How.
    pub kind: LookKind,
    /// When, in milliseconds since the Unix epoch (the latest time for this head and kind).
    pub looked_at_ms: u64,
    /// The review it happened in, when durable.
    #[serde(default)]
    pub review_id: Option<String>,
}

/// Request payload for [`OP_REVIEW_LOOKS_RECORD`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordLayerLooksRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// Looks to record: a known look (same pull request, head, and kind) moves its time forward
    /// and gains what it did not know.
    pub looks: Vec<LayerLook>,
}

/// Response payload for [`OP_REVIEW_LOOKS_RECORD`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordLayerLooksResponse {
    /// Looks added or changed.
    pub recorded: usize,
    /// Heads no longer looked at after pruning (their kept refs can go).
    #[serde(default)]
    pub pruned_heads: Vec<String>,
}

/// Request payload for [`OP_REVIEW_LOOKS_LIST`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListLayerLooksRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// Provider id.
    pub provider: String,
    /// Provider repository.
    pub repository: String,
    /// Pull requests whose looks to list.
    pub numbers: Vec<u64>,
}

/// Response payload for [`OP_REVIEW_LOOKS_LIST`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListLayerLooksResponse {
    /// Their looks, oldest first per pull request.
    pub looks: Vec<LayerLook>,
}

/// One version of a layer, as a comparison names it: its head and where its own change starts.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct VersionPoint {
    /// Head commit.
    pub head_oid: String,
    /// Where the version's own change starts, when known.
    #[serde(default)]
    pub fork_oid: Option<String>,
}

/// One layer to compare: from a version (`None`: the reviewer never looked) to another.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct LayerPair {
    /// Pull request number.
    pub number: u64,
    /// The baseline version.
    #[serde(default)]
    pub from: Option<VersionPoint>,
    /// The version compared with it.
    pub to: VersionPoint,
}

/// What a layer's comparison found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeltaKind {
    /// Same head as the baseline.
    Unchanged,
    /// The baseline's change replayed on the new base is exactly the new version.
    RebasedOnly,
    /// The layer's own change differs; [`LayerDelta::files`] says how.
    Changed,
    /// No baseline: the whole change is new to the reviewer.
    NewToYou,
    /// The rebase could not be taken out (a fork point is unknown or the replay conflicts):
    /// compared head against head.
    WithRebase,
    /// The baseline's commits are not in the local repository.
    Unavailable,
}

impl DeltaKind {
    /// Whether the layer has something to review again.
    #[must_use]
    pub const fn has_changes(self) -> bool {
        matches!(
            self,
            Self::Changed | Self::NewToYou | Self::WithRebase | Self::Unavailable
        )
    }
}

/// How one file changed since a baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileSince {
    /// The same since the baseline.
    Unchanged,
    /// Changed since the baseline, by this many lines.
    Changed {
        /// Lines added since.
        added: u32,
        /// Lines removed since.
        removed: u32,
    },
    /// Not part of the baseline's change; part of the current one.
    New {
        /// Lines added since.
        added: u32,
        /// Lines removed since.
        removed: u32,
    },
    /// Part of the baseline's change; not part of the current one.
    Dropped,
    /// Replay conflicted here; show the full PR diff, without since-baseline counts.
    ReplayConflict,
    /// Not known yet (the layer's comparison is still computing).
    Unknown,
}

impl FileSince {
    /// Whether the file differs from the baseline.
    #[must_use]
    pub const fn changed(self) -> bool {
        matches!(
            self,
            Self::Changed { .. } | Self::New { .. } | Self::Dropped
        )
    }
}

/// One file of a layer's delta.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeltaFile {
    /// Path (on the new side, or the old side for a file the current change no longer has).
    pub path: String,
    /// How it changed.
    pub since: FileSince,
}

/// One layer's comparison: what changed between its two versions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerDelta {
    /// The pair compared.
    pub pair: LayerPair,
    /// What it found.
    pub kind: DeltaKind,
    /// The commit the comparison starts from: the baseline's change replayed on the new fork
    /// point ([`DeltaKind::Changed`], [`DeltaKind::RebasedOnly`]), or the baseline's head
    /// ([`DeltaKind::WithRebase`]). The "only what changed" view diffs from it to the new head.
    #[serde(default)]
    pub replay_oid: Option<String>,
    /// Files that differ from the baseline (every other file of the layer is unchanged), sorted
    /// by path.
    #[serde(default)]
    pub files: Vec<DeltaFile>,
}

impl LayerDelta {
    /// How `path` changed: unchanged when not listed.
    #[must_use]
    pub fn file(&self, path: &str) -> FileSince {
        self.files
            .binary_search_by(|file| file.path.as_str().cmp(path))
            .map_or(FileSince::Unchanged, |index| self.files[index].since)
    }

    /// Lines added and removed since the baseline, over every file.
    #[must_use]
    pub fn totals(&self) -> (u64, u64) {
        self.files
            .iter()
            .fold((0, 0), |(added, removed), file| match file.since {
                FileSince::Changed {
                    added: a,
                    removed: r,
                }
                | FileSince::New {
                    added: a,
                    removed: r,
                } => (added + u64::from(a), removed + u64::from(r)),
                _ => (added, removed),
            })
    }
}

/// Request payload for [`OP_REVIEW_COMPARE_LAYERS`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewCompareLayersRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// Layers to compare (at most [`REVIEW_COMPARE_MAX_LAYERS`]).
    pub pairs: Vec<LayerPair>,
}

/// Response payload for [`OP_REVIEW_COMPARE_LAYERS`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewCompareLayersResponse {
    /// One delta per requested pair, in request order.
    pub deltas: Vec<LayerDelta>,
}

/// Request payload for [`OP_REVIEW_COMPARE_MARKS`]: one file of a layer compared from `replay`
/// (the baseline's change replayed on `fork`) to `head`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReviewCompareMarksRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// Where the current change starts (the old side of the current diff).
    pub fork_oid: String,
    /// The baseline's change replayed on `fork_oid` (see [`LayerDelta::replay_oid`]).
    pub replay_oid: String,
    /// The current head (the new side).
    pub head_oid: String,
    /// Path on the new side.
    pub path: String,
    /// Path on the old side, for renames.
    #[serde(default)]
    pub old_path: Option<String>,
}

/// Lines of the baseline's change that the current change no longer has, shown as a marker at
/// the new-side position where they were.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovedSince {
    /// The marker goes before this new-side line (one past the last line: at the end).
    pub before_new_line: u32,
    /// The removed lines' text.
    pub lines: Vec<String>,
}

/// An inclusive range of lines.
pub type LineSpan = (u32, u32);

/// Response payload for [`OP_REVIEW_COMPARE_MARKS`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewCompareMarksResponse {
    /// New-side lines new since the baseline, sorted, disjoint.
    pub new_lines: Vec<LineSpan>,
    /// Old-side lines of the current diff removed since the baseline (they were unchanged in
    /// the baseline's change), sorted, disjoint.
    pub newly_removed: Vec<LineSpan>,
    /// Lines of the baseline's own change removed since, by position.
    pub removed_since: Vec<RemovedSince>,
    /// New-side lines new since the baseline that replaced a line there, with that line's text
    /// in the baseline, sorted by line: the words that changed can be marked.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub was: Vec<(u32, String)>,
    /// The file was too large to mark; nothing is marked.
    #[serde(default)]
    pub too_large: bool,
}

/// Request payload for [`OP_REVIEW_VERSIONS_KEEP`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewVersionsKeepRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// Commits to keep under [`LOOK_REF_NAMESPACE`] (those missing locally are skipped).
    pub keep: Vec<String>,
    /// Commits to stop keeping.
    #[serde(default)]
    pub release: Vec<String>,
}

/// Response payload for [`OP_REVIEW_VERSIONS_KEEP`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewVersionsKeepResponse {
    /// Refs written.
    pub kept: usize,
    /// Refs removed.
    pub released: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_delta_reports_unlisted_files_as_unchanged_and_sums_its_lines() {
        let delta = LayerDelta {
            pair: LayerPair {
                number: 1,
                from: None,
                to: VersionPoint {
                    head_oid: "b".into(),
                    fork_oid: None,
                },
            },
            kind: DeltaKind::Changed,
            replay_oid: Some("r".into()),
            files: vec![
                DeltaFile {
                    path: "a.rs".into(),
                    since: FileSince::Changed {
                        added: 3,
                        removed: 1,
                    },
                },
                DeltaFile {
                    path: "c.rs".into(),
                    since: FileSince::New {
                        added: 2,
                        removed: 0,
                    },
                },
                DeltaFile {
                    path: "d.rs".into(),
                    since: FileSince::Dropped,
                },
            ],
        };
        assert_eq!(delta.file("b.rs"), FileSince::Unchanged);
        assert!(delta.file("d.rs").changed());
        assert_eq!(delta.totals(), (5, 1));
        assert_eq!(LookKind::parse("pass"), Some(LookKind::Pass));
        assert!(!LookKind::Opened.engaged() && LookKind::Reviewed.engaged());
    }
}
