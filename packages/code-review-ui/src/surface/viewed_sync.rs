//! Keeping viewed marks written elsewhere while the review is open.
//!
//! The review saves its whole workspace (presentation, review pass, viewed state). Viewed marks
//! can also change while it is open: the assistant's `code_review.mark_viewed`, an AI card, or
//! another app on the same review write them to the store. Every save and every reload of the
//! stored viewed state therefore merges three versions per file and per thread:
//!
//! * **base**: the viewed state as last read from or written to the store;
//! * **local**: the review's viewed state now;
//! * **stored**: the viewed state in the store now.
//!
//! A file (or thread) the reviewer changed since the base keeps the reviewer's state; every
//! other one takes the stored state. So a save never undoes a mark written elsewhere, and a
//! reload never drops a mark the reviewer made that is still being saved.

use bcode_code_review_models::{ReviewWorkspace, ThreadViewMark, ViewedMark};
use std::collections::{BTreeMap, BTreeSet};

/// The viewed state as the review last read or wrote it.
#[derive(Debug, Clone, Default)]
pub struct Tracking {
    /// An unsaved review's viewed files, marks and threads as last stored for its target.
    pub transient: Option<(BTreeSet<String>, Vec<ViewedMark>, Vec<ThreadViewMark>)>,
    /// The viewed state as last read from or written to the store.
    pub base: Option<Viewed>,
    /// The viewed state being saved, while a save runs.
    pub saving: Option<Viewed>,
}

/// A workspace's viewed state: viewed paths, viewed marks and viewed threads.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Viewed {
    /// Viewed display paths.
    pub files: BTreeSet<String>,
    /// Viewed marks.
    pub marks: Vec<ViewedMark>,
    /// Viewed threads.
    pub threads: Vec<ThreadViewMark>,
}

impl Viewed {
    /// The viewed state of `workspace`.
    #[must_use]
    pub fn of(workspace: &ReviewWorkspace) -> Self {
        Self {
            files: workspace.viewed_files.clone(),
            marks: workspace.viewed_marks.clone(),
            threads: workspace.viewed_threads.clone(),
        }
    }

    /// Write this viewed state into `workspace`.
    pub fn apply(&self, workspace: &mut ReviewWorkspace) {
        workspace.viewed_files.clone_from(&self.files);
        workspace.viewed_marks.clone_from(&self.marks);
        workspace.viewed_threads.clone_from(&self.threads);
    }

    /// One path's state: whether it is listed viewed, and its marks.
    fn path(&self, path: &str) -> (bool, Vec<&ViewedMark>) {
        let mut marks: Vec<&ViewedMark> =
            self.marks.iter().filter(|mark| mark.path == path).collect();
        marks.sort();
        (self.files.contains(path), marks)
    }

    fn thread(&self, id: &str) -> Option<&ThreadViewMark> {
        self.threads.iter().find(|mark| mark.thread_id == id)
    }

    fn paths(&self) -> impl Iterator<Item = &str> {
        self.files
            .iter()
            .map(String::as_str)
            .chain(self.marks.iter().map(|mark| mark.path.as_str()))
    }
}

/// Merge `local` and `stored` from their common `base`: per file and per thread, the local
/// state where it differs from the base, else the stored state.
#[must_use]
pub fn merge(base: &Viewed, local: &Viewed, stored: &Viewed) -> Viewed {
    let paths: BTreeSet<&str> = base
        .paths()
        .chain(local.paths())
        .chain(stored.paths())
        .collect();
    let mut merged = Viewed::default();
    for path in paths {
        let mine = local.path(path);
        let (listed, marks) = if mine == base.path(path) {
            stored.path(path)
        } else {
            mine
        };
        if listed {
            merged.files.insert(path.to_owned());
        }
        merged.marks.extend(marks.into_iter().cloned());
    }
    merged.marks.sort();
    merged.marks.dedup();
    let threads: BTreeSet<&str> = base
        .threads
        .iter()
        .chain(&local.threads)
        .chain(&stored.threads)
        .map(|mark| mark.thread_id.as_str())
        .collect();
    let mut kept: BTreeMap<&str, ThreadViewMark> = BTreeMap::new();
    for id in threads {
        let mine = local.thread(id);
        let chosen = if mine == base.thread(id) {
            stored.thread(id)
        } else {
            mine
        };
        if let Some(mark) = chosen {
            kept.insert(id, mark.clone());
        }
    }
    // Keep the local order of threads, then the ones only stored.
    for mark in local.threads.iter().chain(&stored.threads) {
        if let Some(mark) = kept.remove(mark.thread_id.as_str()) {
            merged.threads.push(mark);
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn viewed(paths: &[&str]) -> Viewed {
        Viewed {
            files: paths.iter().map(|path| (*path).to_owned()).collect(),
            marks: paths
                .iter()
                .map(|path| ViewedMark {
                    path: (*path).to_owned(),
                    old_oid: Some("a".to_owned()),
                    new_oid: Some("b".to_owned()),
                    patch_id: None,
                })
                .collect(),
            threads: Vec::new(),
        }
    }

    #[test]
    fn a_mark_written_elsewhere_survives_a_save_of_unrelated_changes() {
        let base = viewed(&["a.rs"]);
        let local = viewed(&["a.rs"]);
        let stored = viewed(&["a.rs", "b.rs", "c.rs"]);
        assert_eq!(merge(&base, &local, &stored), stored);
    }

    #[test]
    fn the_reviewers_own_changes_win_for_the_files_they_touched() {
        let base = viewed(&["a.rs", "b.rs"]);
        // The reviewer unmarked b and marked d; elsewhere c was marked and a unmarked.
        let local = viewed(&["a.rs", "d.rs"]);
        let stored = viewed(&["b.rs", "c.rs"]);
        assert_eq!(
            merge(&base, &local, &stored),
            viewed(&["c.rs", "d.rs"]),
            "a unmarked elsewhere, b unmarked here, c marked elsewhere, d marked here"
        );
    }

    #[test]
    fn threads_merge_by_id() {
        let mark = |id: &str, at: u64| ThreadViewMark {
            thread_id: id.to_owned(),
            comment_ids: vec!["c1".to_owned()],
            viewed_at_ms: at,
        };
        let base = Viewed {
            threads: vec![mark("t1", 1)],
            ..Viewed::default()
        };
        let local = Viewed {
            threads: vec![mark("t1", 1), mark("t2", 2)],
            ..Viewed::default()
        };
        let stored = Viewed {
            threads: vec![mark("t3", 3)],
            ..Viewed::default()
        };
        let merged = merge(&base, &local, &stored);
        let ids: Vec<&str> = merged
            .threads
            .iter()
            .map(|mark| mark.thread_id.as_str())
            .collect();
        assert_eq!(ids, vec!["t2", "t3"], "t1 was unviewed elsewhere");
    }
}
