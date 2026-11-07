//! Line counts of working tree changes.

use std::collections::BTreeMap;

use bcode_git::models::LineStats;
use serde::{Deserialize, Serialize};

/// How a worktree's line counts are measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

/// Every way a worktree's lines are counted, for all the clients that show it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modes {
    /// [`CountsMode::Total`] is wanted.
    pub total: bool,
    /// [`CountsMode::Split`] is wanted.
    pub split: bool,
}

impl Modes {
    /// Counting for a total only.
    pub const fn total() -> Self {
        Self {
            total: true,
            split: false,
        }
    }

    /// Also count as `mode`.
    pub const fn add(&mut self, mode: CountsMode) {
        match mode {
            CountsMode::Off => {}
            CountsMode::Total => self.total = true,
            CountsMode::Split => self.split = true,
        }
    }

    /// Whether lines are counted at all.
    pub const fn any(self) -> bool {
        self.total || self.split
    }
}
