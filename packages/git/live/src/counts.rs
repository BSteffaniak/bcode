//! Line counts of working tree changes.

use std::collections::BTreeMap;

use bcode_git::models::LineStats;

/// How a worktree's line counts are measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CountsMode {
    /// Not measured.
    #[default]
    Off,
    /// Staged and unstaged changes together, against `HEAD`.
    Total,
    /// Staged and unstaged changes apart.
    Split,
}

/// Lines changed in working tree files, by path. Untracked files count as unstaged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum LineCounts {
    /// Not measured.
    #[default]
    Off,
    /// Staged and unstaged changes together, against `HEAD`.
    Total(BTreeMap<String, LineStats>),
    /// Staged and unstaged changes apart.
    Split {
        /// `HEAD` to index.
        staged: BTreeMap<String, LineStats>,
        /// Index to worktree, untracked files included.
        unstaged: BTreeMap<String, LineStats>,
    },
}

impl LineCounts {
    /// Every count summed.
    #[must_use]
    pub fn total(&self) -> LineStats {
        match self {
            Self::Off => LineStats::default(),
            Self::Total(total) => total.values().copied().sum(),
            Self::Split { staged, unstaged } => staged
                .values()
                .chain(unstaged.values())
                .copied()
                .sum::<LineStats>(),
        }
    }
}
